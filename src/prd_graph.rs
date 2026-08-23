//! Deterministic parser and in-memory compiler for numbered implementation PRDs.
//!
//! The compiler only consumes task headers, task metadata, and the small set of
//! explicit shared dependency declarations used by the implementation PRD. It
//! never writes controller state or infers dependencies from task titles.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const MAX_PRD_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrdTask {
    pub number: u32,
    pub id: String,
    pub mode: String,
    pub title: String,
    pub completed: bool,
    pub owner: String,
    pub depends_on: Vec<String>,
    pub instruction: String,
    pub acceptance: String,
    /// Human-authored completion notes. Evidence is retained as PRD metadata,
    /// but it is deliberately not used to derive graph nodes or dependencies.
    pub evidence: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrdGraphSummary {
    pub selected_tasks: usize,
    pub node_count: usize,
    pub edge_count: usize,
    pub wave_count: usize,
    pub initial_ready_nodes: usize,
}

#[derive(Clone, Debug, Default)]
struct SharedTaskDefaults {
    dependencies: Vec<String>,
    guidance: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrdGraphPreview {
    pub graph: Value,
    pub summary: PrdGraphSummary,
    pub from_id: String,
    pub through_id: String,
    pub task_count: usize,
    pub node_count: usize,
    pub edge_count: usize,
    pub initial_ready_nodes: usize,
    pub graph_hash: String,
}

#[derive(Clone, Debug)]
struct ParsedHeader {
    number: u32,
    id: String,
    mode: String,
    title: String,
    completed: bool,
}

#[derive(Clone, Debug)]
struct ParsedId {
    id: String,
    number: u32,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct TaskBuilder {
    number: u32,
    id: String,
    mode: String,
    title: String,
    completed: bool,
    owner: Option<String>,
    depends_on: Option<Vec<String>>,
    dependency_context: Vec<String>,
    instruction_parts: Vec<String>,
    acceptance: Option<String>,
    evidence: Option<String>,
    header_line: usize,
}

impl TaskBuilder {
    fn from_header(header: ParsedHeader, header_line: usize) -> Self {
        Self {
            number: header.number,
            id: header.id,
            mode: header.mode,
            title: header.title,
            completed: header.completed,
            owner: None,
            depends_on: None,
            dependency_context: Vec::new(),
            instruction_parts: Vec::new(),
            acceptance: None,
            evidence: None,
            header_line,
        }
    }

    fn finish(self) -> Result<PrdTask> {
        // The real PRD intentionally uses concise group task blocks and
        // selectively omits metadata. Normalize omitted values first; explicit
        // empty metadata remains an authoring error.
        let owner = match self.owner {
            Some(value) if !value.trim().is_empty() => value.trim().to_owned(),
            Some(_) => bail!(
                "Owner metadata must not be empty on line {}",
                self.header_line
            ),
            None => "cross-project".to_owned(),
        };
        let depends_on = match self.depends_on {
            Some(value) => value,
            None => Vec::new(),
        };
        let mut instruction = if self.instruction_parts.is_empty() {
            self.title.clone()
        } else {
            self.instruction_parts
                .iter()
                .map(String::as_str)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        };
        for context in &self.dependency_context {
            instruction = append_context(&instruction, &format!("Dependency gate: {context}"));
        }
        let mut acceptance = match self.acceptance {
            Some(value) if !value.trim().is_empty() => value.trim().to_owned(),
            Some(_) => bail!(
                "Acceptance metadata must not be empty on line {}",
                self.header_line
            ),
            None => instruction.clone(),
        };
        for context in &self.dependency_context {
            acceptance = append_context(&acceptance, &format!("Dependency gate: {context}"));
        }
        for (field, value) in [
            ("owner", owner.as_str()),
            ("title", self.title.as_str()),
            ("mode", self.mode.as_str()),
            ("instruction", instruction.as_str()),
            ("acceptance", acceptance.as_str()),
        ] {
            reject_secret_shaped_string(value, field)?;
        }
        if let Some(evidence) = &self.evidence {
            reject_secret_shaped_string(evidence, "evidence")?;
        }
        for dependency in &depends_on {
            reject_secret_shaped_string(dependency, "depends_on")?;
        }
        Ok(PrdTask {
            number: self.number,
            id: self.id,
            mode: self.mode,
            title: self.title,
            completed: self.completed,
            owner,
            depends_on,
            instruction,
            acceptance,
            evidence: self.evidence,
        })
    }
}

/// Parse numbered task headers and explicit metadata. Group declarations are
/// deliberately limited to task-ID references in the approved PRD prose.
pub(crate) fn parse_numbered_prd(markdown: &str) -> Result<Vec<PrdTask>> {
    if markdown.len() > MAX_PRD_BYTES {
        bail!(
            "PRD markdown exceeds maximum size of {MAX_PRD_BYTES} bytes (got {} bytes)",
            markdown.len()
        );
    }
    reject_secret_shaped_string(markdown, "prd markdown")?;
    let mut tasks = Vec::new();
    let mut current: Option<TaskBuilder> = None;
    for (line_index, line) in markdown.lines().enumerate() {
        let line_number = line_index + 1;
        if let Some(header) = parse_header(line)
            .with_context(|| format!("parse PRD task header on line {line_number}"))?
        {
            if let Some(builder) = current.take() {
                tasks.push(builder.finish()?);
            }
            if tasks.iter().any(|task: &PrdTask| task.id == header.id) {
                bail!(
                    "duplicate PRD task id `{}` on line {line_number}",
                    header.id
                );
            }
            if tasks
                .iter()
                .any(|task: &PrdTask| task.number == header.number)
            {
                bail!(
                    "duplicate PRD task number {} on line {line_number}",
                    header.number
                );
            }
            current = Some(TaskBuilder::from_header(header, line_number));
            continue;
        }
        let Some(builder) = current.as_mut() else {
            continue;
        };
        if let Some((label, value)) = labeled_bullet(line) {
            match label.as_str() {
                "owner" => {
                    if builder.owner.is_some() {
                        bail!("duplicate Owner metadata on line {line_number}");
                    }
                    if value.trim().is_empty() {
                        bail!("Owner metadata must not be empty on line {line_number}");
                    }
                    builder.owner = Some(value.trim().to_owned());
                }
                "depends on" => {
                    if builder.depends_on.is_some() {
                        bail!("duplicate Depends on metadata on line {line_number}");
                    }
                    builder.depends_on = Some(expand_dependencies(&value).with_context(|| {
                        format!("parse dependencies for task on line {line_number}")
                    })?);
                    if let Some(qualifier) = dependency_qualifier(&value) {
                        builder.dependency_context.push(qualifier);
                    }
                }
                "acceptance" | "acceptance criteria" => {
                    if builder.acceptance.is_some() {
                        bail!("duplicate Acceptance metadata on line {line_number}");
                    }
                    if value.trim().is_empty() {
                        bail!("Acceptance metadata must not be empty on line {line_number}");
                    }
                    builder.acceptance = Some(value.trim().to_owned());
                }
                "instruction" => {
                    if value.trim().is_empty() {
                        bail!("Instruction metadata must not be empty on line {line_number}");
                    }
                    builder.instruction_parts.push(value.trim().to_owned());
                }
                "evidence" => {
                    if builder.evidence.is_some() {
                        bail!("duplicate Evidence metadata on line {line_number}");
                    }
                    if value.trim().is_empty() {
                        bail!("Evidence metadata must not be empty on line {line_number}");
                    }
                    builder.evidence = Some(value.trim().to_owned());
                }
                other => bail!("unknown PRD metadata label `{other}` on line {line_number}"),
            }
        } else if let Some(instruction) = unlabeled_instruction_bullet(line) {
            builder.instruction_parts.push(instruction);
        }
    }
    if let Some(builder) = current.take() {
        tasks.push(builder.finish()?);
    }
    if tasks.is_empty() {
        bail!("PRD does not contain any numbered INT task headers");
    }
    validate_contiguous_numbers(&tasks)?;
    let defaults = shared_task_defaults(markdown)?;
    for task in &mut tasks {
        if let Some(default) = defaults.get(&task.mode.to_ascii_uppercase()) {
            for dependency in &default.dependencies {
                if !task.depends_on.contains(dependency) {
                    task.depends_on.push(dependency.clone());
                }
            }
            if !default.guidance.is_empty() {
                task.instruction = append_context(&task.instruction, &default.guidance);
                task.acceptance = append_context(&task.acceptance, &default.guidance);
            }
        }
    }
    Ok(tasks)
}

/// Compile an inclusive task range into an execution graph preview. Dependencies
/// below the range must be completed; dependencies in the range are rewired to
/// verifier nodes; dependencies above the range are rejected.
pub(crate) fn compile_prd(
    markdown: &str,
    source_ref: &str,
    from_id: &str,
    through_id: &str,
    parent_hash: Option<&str>,
) -> Result<PrdGraphPreview> {
    let source_ref = source_ref.trim();
    if source_ref.is_empty() {
        bail!("PRD source_ref must not be empty");
    }
    reject_secret_shaped_string(source_ref, "source_ref")?;
    if let Some(parent_hash) = parent_hash {
        validate_hash(parent_hash, "parent_hash")?;
    }
    let tasks = parse_numbered_prd(markdown)?;
    let (from_id, from_number) = compile_range_id(from_id, "from_id")?;
    let (through_id, through_number) = compile_range_id(through_id, "through_id")?;
    if from_number > through_number {
        bail!("from_id `{from_id}` must not be after through_id `{through_id}`");
    }
    let by_number = tasks
        .iter()
        .map(|task| (task.number, task))
        .collect::<BTreeMap<_, _>>();
    if !by_number.contains_key(&from_number) {
        bail!("from_id `{from_id}` was not found");
    }
    if !by_number.contains_key(&through_number) {
        bail!("through_id `{through_id}` was not found");
    }
    let selected = (from_number..=through_number)
        .map(|number| {
            by_number.get(&number).copied().with_context(|| {
                format!(
                    "requested range includes missing task `{}`",
                    format_task_id(number)
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(task) = selected.iter().find(|task| task.completed) {
        bail!("selected task `{}` is already marked complete", task.id);
    }
    let by_id = tasks
        .iter()
        .map(|task| (task.id.as_str(), task))
        .collect::<BTreeMap<_, _>>();
    let selected_numbers = (from_number..=through_number).collect::<BTreeSet<_>>();
    let mut predecessors = BTreeMap::<String, Vec<String>>::new();
    let mut nodes = Vec::<Value>::with_capacity(selected.len() * 2);
    for task in &selected {
        let mut deps = Vec::new();
        for dependency_id in &task.depends_on {
            let dependency = by_id.get(dependency_id.as_str()).with_context(|| {
                format!(
                    "task `{}` depends on missing PRD task `{dependency_id}`",
                    task.id
                )
            })?;
            if dependency.number < from_number {
                if !dependency.completed {
                    bail!(
                        "dependency `{dependency_id}` for `{}` is below the selected range but incomplete",
                        task.id
                    );
                }
            } else if selected_numbers.contains(&dependency.number) {
                deps.push(verifier_id(&dependency.id));
            } else {
                bail!(
                    "task `{}` dependency `{dependency_id}` is above selected range ending at `{through_id}`",
                    task.id
                );
            }
        }
        deps.sort();
        deps.dedup();
        predecessors.insert(task.id.clone(), deps.clone());
        predecessors.insert(verifier_id(&task.id), vec![task.id.clone()]);
        let deferred = task.mode.eq_ignore_ascii_case("DEFERRED");
        let deferred_guard = deferred_gate_instruction(deferred);
        let implementation_instruction = deferred_guard
            .map(|guard| append_context(&task.instruction, guard))
            .unwrap_or_else(|| task.instruction.clone());
        let implementation_acceptance = task.acceptance.clone();
        let mut implementation = json!({
            "id": task.id,
            "title": task.title,
            "kind": "codex",
            "node_type": "implementation",
            "capability": "code.generate",
            "objective": implementation_instruction,
            "instruction": implementation_instruction,
            "acceptance": implementation_acceptance,
            "owner": task.owner,
            "prd": {"id": task.id, "number": task.number, "mode": task.mode},
            "depends_on": deps,
        });
        let verifier_instruction = format!(
            "Verify task {} against its acceptance criteria: {}{}",
            task.id,
            task.acceptance,
            deferred_guard
                .map(|guard| format!("\n\n{guard}"))
                .unwrap_or_default()
        );
        let mut verifier = json!({
            "id": verifier_id(&task.id),
            "title": format!("Verify {} — {}", task.id, task.title),
            "kind": "codex",
            "node_type": "verification",
            "capability": "test.verify",
            "objective": task.acceptance,
            "instruction": verifier_instruction,
            "acceptance": task.acceptance,
            "owner": task.owner,
            "prd": {"id": task.id, "number": task.number, "mode": task.mode},
            "depends_on": [task.id],
        });
        if deferred {
            implementation["external_gates"] = json!(["security_review"]);
            implementation["deferred"] = Value::Bool(true);
            verifier["external_gates"] = json!(["security_review"]);
            verifier["deferred"] = Value::Bool(true);
        }
        nodes.push(implementation);
        nodes.push(verifier);
    }
    let waves = topological_waves(&predecessors)?;
    let mut ids_by_wave = BTreeMap::<u32, Vec<String>>::new();
    for (id, wave) in &waves {
        ids_by_wave.entry(*wave).or_default().push(id.clone());
    }
    for ids in ids_by_wave.values_mut() {
        ids.sort();
    }
    let mut task_numbers = BTreeMap::<String, String>::new();
    for (wave, ids) in &ids_by_wave {
        for (position, id) in ids.iter().enumerate() {
            task_numbers.insert(id.clone(), format!("{wave}.{}", position + 1));
        }
    }
    let mut edges = predecessors
        .iter()
        .flat_map(|(to, froms)| {
            froms
                .iter()
                .map(|from| json!({"from": from, "to": to, "condition": "success"}))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    edges.sort_by(|a, b| {
        (
            string_field(a, "from"),
            string_field(a, "to"),
            string_field(a, "condition"),
        )
            .cmp(&(
                string_field(b, "from"),
                string_field(b, "to"),
                string_field(b, "condition"),
            ))
    });
    edges.dedup();
    let mut nodes_by_id = nodes
        .into_iter()
        .map(|node| (string_field(&node, "id").to_owned(), node))
        .collect::<BTreeMap<_, _>>();
    for node in nodes_by_id.values_mut() {
        let id = string_field(node, "id");
        let wave = waves[id];
        let ids = &ids_by_wave[&wave];
        let parallel = ids.len() > 1;
        node["execution"] = json!({
            "mode": if parallel {"parallel"} else {"sequential"},
            "wave": wave,
            "task_number": task_numbers[id],
            "parallel_group": if parallel {Value::String(format!("wave-{wave}"))} else {Value::Null},
        });
    }
    let nodes = selected
        .iter()
        .flat_map(|task| [task.id.clone(), verifier_id(&task.id)])
        .map(|id| {
            nodes_by_id
                .remove(&id)
                .expect("node inserted for every selected task")
        })
        .collect::<Vec<_>>();
    let flow_waves = ids_by_wave
        .iter()
        .map(|(wave, ids)| {
            let parallel = ids.len() > 1;
            json!({
                "wave": wave,
                "mode": if parallel {"parallel"} else {"sequential"},
                "parallel_group": if parallel {Value::String(format!("wave-{wave}"))} else {Value::Null},
                "nodes": ids,
            })
        })
        .collect::<Vec<_>>();
    let wave_count = ids_by_wave.len();
    let initial_ready_nodes = ids_by_wave.get(&1).map_or(0, Vec::len);
    let summary = PrdGraphSummary {
        selected_tasks: selected.len(),
        node_count: nodes.len(),
        edge_count: edges.len(),
        wave_count,
        initial_ready_nodes,
    };
    let summary_value = json!({
        "selected_tasks": summary.selected_tasks,
        "node_count": summary.node_count,
        "edge_count": summary.edge_count,
        "wave_count": summary.wave_count,
        "initial_ready_nodes": summary.initial_ready_nodes,
    });
    let mut canonical_tasks = tasks
        .iter()
        .map(|task| {
            json!({
                "number": task.number,
                "id": task.id,
                "mode": task.mode,
                "title": task.title,
                "completed": task.completed,
                "owner": task.owner,
                "depends_on": task.depends_on,
                "instruction": task.instruction,
                "acceptance": task.acceptance,
                "evidence": task.evidence,
            })
        })
        .collect::<Vec<_>>();
    canonical_tasks.sort_by(|a, b| string_field(a, "id").cmp(string_field(b, "id")));
    let source_hash = fractal_contracts::canonical_sha256(&Value::Array(canonical_tasks))
        .map_err(|error| anyhow::anyhow!("hash PRD source: {error}"))?;
    let source = json!({
        "kind": "numbered_markdown_prd",
        "source_ref": source_ref,
        "source_hash": source_hash,
        "from_id": from_id,
        "through_id": through_id,
    });
    let identity = json!({
        "schema": "fractal.execution_graph.v1",
        "parent_graph": parent_hash,
        "source": source,
        "nodes": nodes,
        "edges": edges,
        "execution_flow": {"schema": "fractal.execution_flow.v1", "waves": flow_waves},
        "summary": summary_value,
    });
    let identity_hash = fractal_contracts::canonical_sha256(&identity)
        .map_err(|error| anyhow::anyhow!("hash PRD graph identity: {error}"))?;
    let graph_id = format!("fg_prd_{}", &identity_hash["sha256:".len()..][..20]);
    let mut graph = json!({
        "schema": "fractal.execution_graph.v1",
        "graph_id": graph_id,
        "parent_graph": parent_hash,
        "source": source,
        "nodes": nodes,
        "edges": edges,
        "execution_flow": {"schema": "fractal.execution_flow.v1", "waves": flow_waves},
        "summary": summary_value,
    });
    let graph_hash = fractal_contracts::canonical_sha256(&graph)
        .map_err(|error| anyhow::anyhow!("hash compiled PRD graph: {error}"))?;
    graph["graph_hash"] = Value::String(graph_hash.clone());
    Ok(PrdGraphPreview {
        graph,
        summary: summary.clone(),
        from_id,
        through_id,
        task_count: summary.selected_tasks,
        node_count: summary.node_count,
        edge_count: summary.edge_count,
        initial_ready_nodes: summary.initial_ready_nodes,
        graph_hash,
    })
}

fn topological_waves(
    predecessors: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, u32>> {
    let ids = predecessors.keys().cloned().collect::<BTreeSet<_>>();
    for (node, deps) in predecessors {
        for dep in deps {
            if !ids.contains(dep) {
                bail!("compiled PRD edge `{dep}` -> `{node}` references a missing node");
            }
        }
    }
    let mut waves = BTreeMap::new();
    while waves.len() < ids.len() {
        let before = waves.len();
        for (node, deps) in predecessors {
            if waves.contains_key(node) || !deps.iter().all(|dep| waves.contains_key(dep)) {
                continue;
            }
            let wave = deps.iter().map(|dep| waves[dep] + 1).max().unwrap_or(1);
            waves.insert(node.clone(), wave);
        }
        if waves.len() == before {
            let cycle = ids
                .iter()
                .filter(|id| !waves.contains_key(*id))
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            bail!("selected PRD dependencies contain a cycle involving: {cycle}");
        }
    }
    Ok(waves)
}

fn shared_task_defaults(markdown: &str) -> Result<BTreeMap<String, SharedTaskDefaults>> {
    let mut defaults = BTreeMap::<String, SharedTaskDefaults>::new();
    for line in markdown.lines() {
        let text = line.trim().trim_matches('*').trim();
        let lower = text.to_ascii_lowercase();
        let mode = if lower.starts_with("all par-") && lower.contains(" tasks depend on ") {
            let rest = &text[4..];
            rest.split_once(" tasks depend on ")
                .map(|(mode, _)| mode.trim().to_ascii_uppercase())
        } else if lower.starts_with("par-f depends on ") {
            Some("PAR-F".to_owned())
        } else if lower.starts_with("par-g begins after ") {
            Some("PAR-G".to_owned())
        } else if lower.starts_with("these tasks may begin only after ") {
            Some("DEFERRED".to_owned())
        } else {
            None
        };
        let Some(mode) = mode else { continue };
        let ids = parse_id_tokens(text)?;
        let default = defaults.entry(mode.clone()).or_default();
        for parsed in ids {
            if !default.dependencies.contains(&parsed.id) {
                default.dependencies.push(parsed.id);
            }
        }
        let guidance = normalize_shared_guidance(text);
        if !guidance.is_empty() && !default.guidance.contains(&guidance) {
            if default.guidance.is_empty() {
                default.guidance = guidance;
            } else {
                default.guidance.push(' ');
                default.guidance.push_str(&guidance);
            }
        }
    }
    Ok(defaults)
}

fn normalize_shared_guidance(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compile_range_id(value: &str, field: &str) -> Result<(String, u32)> {
    let trimmed = value.trim();
    let ids = parse_id_tokens(trimmed)?;
    let [parsed] = ids.as_slice() else {
        bail!("{field} must be exactly one INT identifier")
    };
    if parsed.start != 0 || parsed.end != trimmed.len() {
        bail!("{field} must be exactly one INT identifier");
    }
    Ok((parsed.id.clone(), parsed.number))
}

fn verifier_id(task_id: &str) -> String {
    format!("verify.{task_id}")
}
fn format_task_id(number: u32) -> String {
    format!("INT-{number:03}")
}
fn string_field<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or_default()
}

fn validate_hash(value: &str, field: &str) -> Result<()> {
    let digest = value
        .strip_prefix("sha256:")
        .with_context(|| format!("{field} must start with `sha256:`"))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{field} must contain 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn validate_contiguous_numbers(tasks: &[PrdTask]) -> Result<()> {
    let mut numbers = tasks.iter().map(|task| task.number).collect::<Vec<_>>();
    numbers.sort_unstable();
    for (index, number) in numbers.iter().enumerate() {
        let expected = index as u32 + 1;
        if *number != expected {
            bail!("PRD task numbers must be contiguous starting at 1 (expected {expected}, found {number})");
        }
    }
    for task in tasks {
        let expected = format_task_id(task.number);
        if task.id != expected {
            bail!(
                "PRD task number {} must use id `{expected}` (found `{}`)",
                task.number,
                task.id
            );
        }
    }
    Ok(())
}

fn parse_header(line: &str) -> Result<Option<ParsedHeader>> {
    let Some((number, mut body)) = ordered_line(line) else {
        return Ok(None);
    };
    let completed = if let Some(rest) = body
        .strip_prefix("[x]")
        .or_else(|| body.strip_prefix("[X]"))
    {
        body = rest.trim_start();
        true
    } else if let Some(rest) = body.strip_prefix("[ ]") {
        body = rest.trim_start();
        false
    } else if body.to_ascii_uppercase().contains("INT-") {
        bail!("task header checklist marker `[ ]` or `[x]` is required");
    } else {
        return Ok(None);
    };
    let had_bold = body.starts_with("**");
    if had_bold {
        body = &body[2..];
    }
    body = body.trim_start();
    if body.ends_with("**") {
        body = &body[..body.len() - 2];
    }
    body = body.trim();
    if !body.to_ascii_uppercase().starts_with("INT") {
        return Ok(None);
    }
    let parsed = parse_id_tokens(body)?
        .into_iter()
        .next()
        .context("task header starts with INT but has no valid INT identifier")?;
    if parsed.start != 0 {
        bail!("task identifier must be the first header token")
    }
    if parsed.number != number {
        bail!(
            "task list number {number} does not match identifier `{}`",
            parsed.id
        )
    }
    let mut rest = body[parsed.end..].trim_start();
    if !rest.starts_with('[') {
        bail!("task header mode `[MODE]` is required")
    }
    let close = rest
        .find(']')
        .context("task header mode is missing a closing `]`")?;
    let mode = rest[1..close].trim().to_owned();
    if mode.is_empty() {
        bail!("task header mode must not be empty")
    }
    rest = rest[close + 1..].trim_start();
    let mut separated = false;
    for separator in ["—", "–", "-"] {
        if let Some(after) = rest.strip_prefix(separator) {
            rest = after.trim_start();
            separated = true;
            break;
        }
    }
    if !separated {
        if let Some(after) = rest.strip_prefix(':') {
            rest = after.trim_start();
            separated = true;
        }
    }
    if !separated {
        bail!("task header title separator `—` is required")
    }
    let title = rest.trim().trim_end_matches('*').trim().to_owned();
    if title.is_empty() {
        bail!("task header title must not be empty")
    }
    if !had_bold && body.contains("**") {
        bail!("task header has unbalanced Markdown bold markers")
    }
    Ok(Some(ParsedHeader {
        number,
        id: parsed.id,
        mode,
        title,
        completed,
    }))
}

fn ordered_line(line: &str) -> Option<(u32, &str)> {
    let mut body = line.trim_start();
    while let Some(rest) = body.strip_prefix('#') {
        body = rest.trim_start();
    }
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let punctuation = body.as_bytes().get(digits).copied()?;
    if punctuation != b'.' && punctuation != b')' {
        return None;
    }
    if !body
        .as_bytes()
        .get(digits + 1)
        .is_some_and(u8::is_ascii_whitespace)
    {
        return None;
    }
    Some((
        body[..digits].parse().ok()?,
        body[digits + 2..].trim_start(),
    ))
}

fn labeled_bullet(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let without = trimmed
        .strip_prefix('-')
        .or_else(|| trimmed.strip_prefix('*'))?
        .trim_start();
    if !without.starts_with("**") {
        return None;
    }
    let after_open = &without[2..];
    let end = after_open.find("**")?;
    let label = after_open[..end].trim().trim_end_matches(':').trim();
    if label.is_empty() {
        return None;
    }
    let mut rest = after_open[end + 2..].trim_start();
    if let Some(stripped) = rest.strip_prefix(':') {
        rest = stripped.trim_start();
    }
    Some((label.to_ascii_lowercase(), rest.trim().to_owned()))
}

fn unlabeled_instruction_bullet(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let without = trimmed
        .strip_prefix('-')
        .or_else(|| trimmed.strip_prefix('*'))?
        .trim_start();
    if without.starts_with("**") {
        return None;
    }
    let text = without.trim().trim_matches('*').trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn dependency_qualifier(spec: &str) -> Option<String> {
    let normalized = spec.trim().trim_matches('*').trim();
    let lower = normalized.to_ascii_lowercase();
    let suffix = " passing deterministic usefulness gate";
    let prefix = lower.strip_suffix(suffix)?;
    if parse_id_tokens(prefix.trim()).ok()?.len() == 1 {
        Some("passing deterministic usefulness gate".to_owned())
    } else {
        None
    }
}

fn expand_dependencies(spec: &str) -> Result<Vec<String>> {
    let spec = spec.trim().trim_matches('*').trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut expanded = Vec::new();
    for segment in spec.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            bail!("dependency list contains an empty comma item")
        }
        let ids = parse_id_tokens(segment)?;
        match ids.as_slice() {
            [] => bail!("unknown dependency token `{segment}`"),
            [single] => {
                let trailing = segment[single.end..].trim();
                let allowed_qualifier = trailing.is_empty()
                    || trailing.eq_ignore_ascii_case("passing deterministic usefulness gate");
                if !remaining_is_decoration(&segment[..single.start]) || !allowed_qualifier {
                    bail!("unknown dependency token `{segment}`")
                }
                push_unique(&mut expanded, single.id.clone());
            }
            [first, second] => {
                let connector = &segment[first.end..second.start];
                if !range_connector(connector)
                    || !remaining_is_decoration(&segment[..first.start])
                    || !remaining_is_decoration(&segment[second.end..])
                {
                    bail!("unknown dependency token `{segment}`")
                }
                if first.number > second.number {
                    bail!(
                        "dependency range must be ascending: `{}` through `{}`",
                        first.id,
                        second.id
                    )
                }
                if second.number - first.number > 10_000 {
                    bail!("dependency range is too large (maximum 10001 IDs)")
                }
                for number in first.number..=second.number {
                    push_unique(&mut expanded, format_task_id(number));
                }
            }
            _ => bail!("unknown dependency token `{segment}`"),
        }
    }
    Ok(expanded)
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn append_context(base: &str, context: &str) -> String {
    if base.contains(context) {
        base.to_owned()
    } else {
        format!("{base}\n\n{context}")
    }
}

fn deferred_gate_instruction(deferred: bool) -> Option<&'static str> {
    deferred.then_some(
        "Do not begin this deferred task unless evidence of the separate security review approval is present; if the review evidence is absent, refuse or release the task.",
    )
}

fn remaining_is_decoration(value: &str) -> bool {
    value
        .chars()
        .all(|c| c.is_whitespace() || matches!(c, '`' | '*' | '(' | ')'))
}
fn range_connector(value: &str) -> bool {
    let trimmed = value.to_ascii_lowercase();
    let trimmed = trimmed.trim();
    trimmed == "through"
        || trimmed == "to"
        || (trimmed.chars().all(|c| matches!(c, '-' | '–' | '—' | ' '))
            && trimmed.chars().any(|c| matches!(c, '-' | '–' | '—')))
}

fn parse_id_tokens(value: &str) -> Result<Vec<ParsedId>> {
    let upper = value.to_ascii_uppercase();
    let mut tokens = Vec::new();
    let mut search = 0;
    while let Some(relative) = upper[search..].find("INT-") {
        let start = search + relative;
        if start > 0
            && upper[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            search = start + 4;
            continue;
        }
        let digits_start = start + 4;
        let digits_end = digits_start
            + upper[digits_start..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
        if digits_end == digits_start {
            bail!("INT identifier is missing its numeric suffix in `{value}`")
        }
        let raw = &upper[digits_start..digits_end];
        let number = raw
            .parse::<u32>()
            .with_context(|| format!("INT identifier is out of range in `{value}`"))?;
        if number > 999 {
            bail!("INT identifier `{raw}` exceeds three-digit normalization")
        }
        tokens.push(ParsedId {
            id: format_task_id(number),
            number,
            start,
            end: digits_end,
        });
        search = digits_end;
    }
    Ok(tokens)
}

fn reject_secret_shaped_string(value: &str, field: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    for marker in [
        "api_key=",
        "access_token=",
        "password=",
        "private_key=",
        "bearer ",
        "secret=",
    ] {
        if lower.contains(marker) {
            bail!("{field} contains secret-shaped material `{marker}`")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize_selected_task_checkboxes(markdown: &str, from: u32, through: u32) -> String {
        markdown
            .split_inclusive('\n')
            .map(|line| {
                let without_newline = line.strip_suffix('\n').unwrap_or(line);
                let Some((number, body)) = ordered_line(without_newline) else {
                    return line.to_owned();
                };
                if !(from..=through).contains(&number)
                    || !(body.starts_with("[x]") || body.starts_with("[X]"))
                {
                    return line.to_owned();
                }
                let marker = if body.starts_with("[x]") {
                    "[x]"
                } else {
                    "[X]"
                };
                let Some(marker_offset) = without_newline.find(marker) else {
                    return line.to_owned();
                };
                let mut normalized = without_newline.to_owned();
                normalized.replace_range(marker_offset..marker_offset + marker.len(), "[ ]");
                if line.ends_with('\n') {
                    normalized.push('\n');
                }
                normalized
            })
            .collect()
    }

    fn block(
        number: u32,
        complete: bool,
        mode: &str,
        title: &str,
        owner: &str,
        deps: &str,
    ) -> String {
        format!(
            "{number}. [{}] **INT-{number:03} [{mode}] — {title}**\n   - **Owner:** {owner}\n   - **Depends on:** {deps}\n   - Implement {title}.\n   - **Acceptance:** {title} is complete.\n",
            if complete {"x"} else {" "}
        )
    }

    #[test]
    fn parses_explicit_metadata_and_ranges() {
        let text = format!(
            "{}{}",
            block(1, true, "SEQ", "Foundation", "A", "none"),
            block(2, false, "PAR-A", "Adapter", "B", "INT-001")
        );
        let tasks = parse_numbered_prd(&text).unwrap();
        assert_eq!(tasks[0].id, "INT-001");
        assert_eq!(tasks[1].depends_on, vec!["INT-001"]);
    }

    #[test]
    fn completed_evidence_is_retained_without_affecting_graph_topology() {
        let evidence = "verified by the adapter regression suite and fixture replay";
        let with_evidence = format!(
            "{}   - **Evidence:** {evidence}\n{}",
            block(1, true, "SEQ", "Foundation", "A", "none"),
            block(2, false, "SEQ", "Adapter", "B", "INT-001")
        );
        let without_evidence = format!(
            "{}{}",
            block(1, true, "SEQ", "Foundation", "A", "none"),
            block(2, false, "SEQ", "Adapter", "B", "INT-001")
        );
        let parsed = parse_numbered_prd(&with_evidence).unwrap();
        assert_eq!(parsed[0].evidence.as_deref(), Some(evidence));
        assert!(!parsed[0].instruction.contains(evidence));
        assert!(!parsed[0].acceptance.contains(evidence));

        let documented = compile_prd(
            &with_evidence,
            "fixture://evidence",
            "INT-002",
            "INT-002",
            None,
        )
        .unwrap();
        let plain = compile_prd(
            &without_evidence,
            "fixture://evidence",
            "INT-002",
            "INT-002",
            None,
        )
        .unwrap();
        assert_eq!(documented.summary, plain.summary);
        assert_eq!(documented.graph["edges"], plain.graph["edges"]);
        let documented_ids = documented.graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        let plain_ids = plain.graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(documented_ids, plain_ids);
    }

    #[test]
    fn evidence_metadata_is_non_topology_and_unknown_labels_stay_rejected() {
        let incomplete = format!(
            "{}   - **Evidence:** completion note may be retained before checkbox sync\n",
            block(1, false, "SEQ", "Foundation", "A", "none")
        );
        let parsed = parse_numbered_prd(&incomplete).unwrap();
        assert_eq!(
            parsed[0].evidence.as_deref(),
            Some("completion note may be retained before checkbox sync")
        );

        let duplicate = format!(
            "{}   - **Evidence:** first\n   - **Evidence:** second\n",
            block(1, true, "SEQ", "Foundation", "A", "none")
        );
        let error = parse_numbered_prd(&duplicate).unwrap_err().to_string();
        assert!(error.contains("duplicate Evidence metadata"));

        let unknown = format!(
            "{}   - **Completion note:** unsupported\n",
            block(1, true, "SEQ", "Foundation", "A", "none")
        );
        let error = parse_numbered_prd(&unknown).unwrap_err().to_string();
        assert!(error.contains("unknown PRD metadata label `completion note`"));
    }

    #[test]
    fn rewires_dependencies_to_verifiers_and_has_frontier() {
        let text = format!(
            "{}{}{}",
            block(1, true, "SEQ", "One", "A", "none"),
            block(2, false, "PAR-A", "Two", "B", "INT-001"),
            block(3, false, "SEQ", "Three", "A", "INT-002")
        );
        let graph = compile_prd(&text, "fixture://chain", "INT-002", "INT-003", None).unwrap();
        assert_eq!(graph.summary.node_count, 4);
        assert_eq!(graph.summary.initial_ready_nodes, 1);
        let node = |id: &str| {
            graph.graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["id"] == id)
                .unwrap()
        };
        assert_eq!(node("INT-003")["depends_on"], json!(["verify.INT-002"]));
        assert_eq!(node("verify.INT-002")["depends_on"], json!(["INT-002"]));
        assert!(
            graph.graph["execution_flow"]["waves"]
                .as_array()
                .unwrap()
                .len()
                >= 2
        );
    }

    #[test]
    fn hashes_are_stable_for_reordered_source() {
        let a = format!(
            "{}{}{}",
            block(1, true, "SEQ", "One", "A", "none"),
            block(2, false, "SEQ", "Two", "B", "INT-001"),
            block(3, false, "SEQ", "Three", "C", "INT-002")
        );
        let b = format!(
            "{}{}{}",
            block(3, false, "SEQ", "Three", "C", "INT-002"),
            block(1, true, "SEQ", "One", "A", "none"),
            block(2, false, "SEQ", "Two", "B", "INT-001")
        );
        let first = compile_prd(&a, "fixture://stable", "INT-002", "INT-003", None).unwrap();
        let second = compile_prd(&b, "fixture://stable", "INT-002", "INT-003", None).unwrap();
        assert_eq!(first.graph["graph_hash"], second.graph["graph_hash"]);
        assert_eq!(first.graph["graph_id"], second.graph["graph_id"]);
    }

    #[test]
    fn real_prd_compiles_to_exact_dag_and_initial_frontier() {
        let path = "/Users/jamesstar/fractalmaster/docs/2026-08-23_FRACTAL_EVOLVING_INTELLIGENCE_UPGRADE_IMPLEMENTATION_PRD.md";
        let markdown = std::fs::read_to_string(path).expect("dated implementation PRD must exist");
        // The live PRD is updated by workers as they finish tasks. Compile a
        // stable synthetic unfinished range so this structural regression test
        // remains valid while still parsing all real metadata, including
        // completion Evidence bullets.
        let markdown = normalize_selected_task_checkboxes(&markdown, 8, 61);
        let parsed = parse_numbered_prd(&markdown).expect("real PRD task metadata normalizes");
        let parsed_task = |id: &str| parsed.iter().find(|task| task.id == id).unwrap();
        assert!(parsed_task("INT-019")
            .instruction
            .contains("time, project, identity, privacy, and confidence filters"));
        assert!(parsed_task("INT-026")
            .acceptance
            .contains("deterministic and versioned"));
        assert!(parsed_task("INT-033")
            .instruction
            .contains("They may write proposals only"));
        assert!(parsed_task("INT-040")
            .acceptance
            .contains("keyboard, narrow-screen, reduced-motion, and non-WebGL fallbacks"));
        assert!(parsed_task("INT-046")
            .instruction
            .contains("runners integrate as corresponding systems become available"));
        assert!(parsed_task("INT-053")
            .instruction
            .contains("passing deterministic usefulness gate"));
        assert!(parsed_task("INT-053")
            .acceptance
            .contains("passing deterministic usefulness gate"));
        assert!(parsed_task("INT-057")
            .instruction
            .contains("separate security review approves the trust model"));
        let preview =
            compile_prd(&markdown, path, "INT-008", "INT-061", None).expect("real PRD compiles");
        assert_eq!(preview.summary.selected_tasks, 54);
        assert_eq!(preview.summary.node_count, 108);
        assert_eq!(
            preview.graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|node| node["node_type"] == "implementation")
                .count(),
            54
        );
        assert_eq!(
            preview.graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|node| node["node_type"] == "verification")
                .count(),
            54
        );
        let nodes = preview.graph["nodes"].as_array().unwrap();
        let frontier = nodes
            .iter()
            .filter(|node| node["execution"]["wave"] == 1)
            .map(|node| node["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        let expected = [
            "INT-008", "INT-011", "INT-012", "INT-013", "INT-014", "INT-015", "INT-016", "INT-046",
            "INT-047", "INT-048", "INT-049", "INT-050", "INT-051",
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        assert_eq!(frontier, expected);
        let node = |id: &str| nodes.iter().find(|node| node["id"] == id).unwrap();
        assert_eq!(node("INT-009")["depends_on"], json!(["verify.INT-008"]));
        assert_eq!(node("INT-019")["depends_on"], json!(["verify.INT-017"]));
        assert_eq!(
            node("INT-040")["depends_on"],
            json!(["verify.INT-018", "verify.INT-025"])
        );
        assert_eq!(node("INT-057")["depends_on"], json!(["verify.INT-052"]));
        for id in ["INT-057", "INT-058", "INT-059", "INT-060", "INT-061"] {
            let implementation = node(id);
            let verifier = node(&format!("verify.{id}"));
            for deferred in [implementation, verifier] {
                assert_eq!(deferred["deferred"], json!(true));
                assert_eq!(deferred["external_gates"], json!(["security_review"]));
                assert!(deferred["instruction"]
                    .as_str()
                    .unwrap()
                    .contains("refuse or release"));
                assert!(deferred["instruction"]
                    .as_str()
                    .unwrap()
                    .contains("security review"));
            }
        }
        for edge in preview.graph["edges"].as_array().unwrap() {
            assert!(nodes.iter().any(|node| node["id"] == edge["from"]));
            assert!(nodes.iter().any(|node| node["id"] == edge["to"]));
        }
        let mut object = preview.graph.as_object().unwrap().clone();
        let claimed = object.remove("graph_hash").unwrap();
        let recomputed = fractal_contracts::canonical_sha256(&Value::Object(object)).unwrap();
        assert_eq!(claimed, recomputed);
    }

    #[test]
    fn rejects_cycles_and_invalid_secret_or_range() {
        let cycle = format!(
            "{}{}",
            block(1, false, "SEQ", "One", "A", "INT-002"),
            block(2, false, "SEQ", "Two", "A", "INT-001")
        );
        assert!(
            compile_prd(&cycle, "fixture://cycle", "INT-001", "INT-002", None)
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
        assert!(compile_prd(&cycle, "fixture://x", "INT-002", "INT-001", None).is_err());
        assert!(compile_prd(&cycle, "password=bad", "INT-001", "INT-002", None).is_err());
    }

    #[test]
    fn dependency_qualifier_allowlist_is_narrow() {
        assert_eq!(
            expand_dependencies("INT-052 passing deterministic usefulness gate").unwrap(),
            vec!["INT-052"]
        );
        assert!(expand_dependencies("INT-052 arbitrary trailing prose").is_err());
    }
}
