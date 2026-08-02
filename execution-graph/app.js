"use strict";

const NS = "http://www.w3.org/2000/svg";
const state = { data: null, view: "overview", selected: null, initialized: false, pausing: false, estimatedSaved: 0 };
const statusNames = { complete: "Complete", active: "In progress", incomplete: "Incomplete" };

const overviewPositions = {
  M0: [145, 375], M1: [390, 125], M2: [390, 375], M3: [390, 625],
  M4: [650, 375], M5: [890, 375], M6: [1125, 180], M7: [1125, 550],
  M8: [1370, 270], M9: [1370, 510], M10: [1600, 390]
};

function svgElement(name, attrs = {}) {
  const element = document.createElementNS(NS, name);
  Object.entries(attrs).forEach(([key, value]) => element.setAttribute(key, value));
  return element;
}

function splitTitle(text, max = 25) {
  if (text.length <= max) return [text];
  const words = text.split(/\s+/);
  const lines = [""];
  for (const word of words) {
    const last = lines.length - 1;
    if (`${lines[last]} ${word}`.trim().length > max && lines[last]) lines.push(word);
    else lines[last] = `${lines[last]} ${word}`.trim();
    if (lines.length === 2 && lines[1].length > max) break;
  }
  return lines.slice(0, 2);
}

function truncateLabel(text, max = 18) {
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}

function edgePath(from, to, width, taskView) {
  const x1 = from[0] + width / 2;
  const x2 = to[0] - width / 2;
  const bend = Math.max(40, Math.abs(x2 - x1) * .48);
  if (taskView && x2 < x1) {
    const y = Math.max(from[1], to[1]) + 70;
    return `M ${x1} ${from[1]} C ${x1 + 28} ${from[1]}, ${x1 + 28} ${y}, ${from[0]} ${y} L ${to[0]} ${y} C ${x2 - 28} ${y}, ${x2 - 28} ${to[1]}, ${x2} ${to[1]}`;
  }
  return `M ${x1} ${from[1]} C ${x1 + bend} ${from[1]}, ${x2 - bend} ${to[1]}, ${x2} ${to[1]}`;
}

function renderGraph() {
  const svg = document.getElementById("graph");
  svg.replaceChildren();
  const taskView = state.view !== "overview";
  const group = taskView ? state.data.groups.find(item => item.id === state.view) : null;
  const nodes = taskView ? group.tasks : state.data.overview.nodes;
  const edges = taskView ? group.edges : state.data.overview.edges;
  const width = taskView ? 238 : 190;
  const height = taskView ? 92 : 100;
  const positions = {};
  const displayWaves = {};

  if (taskView) {
    const fallbackWave = new Map();
    const unresolved = new Set(nodes.map(node => node.id));
    let wave = 1;
    while (unresolved.size) {
      const ready = [...unresolved].filter(id =>
        edges.filter(edge => edge.to === id && edge.condition !== "failure")
          .every(edge => fallbackWave.has(edge.from)));
      const frontier = ready.length ? ready : [...unresolved];
      frontier.forEach(id => { fallbackWave.set(id, wave); unresolved.delete(id); });
      wave += 1;
    }
    const waveGroups = new Map();
    nodes.forEach(node => {
      const declaredWave = Number(node.execution?.wave);
      const nodeWave = Number.isInteger(declaredWave) && declaredWave >= 0
        ? declaredWave
        : fallbackWave.get(node.id) || 1;
      displayWaves[node.id] = nodeWave;
      if (!waveGroups.has(nodeWave)) waveGroups.set(nodeWave, []);
      waveGroups.get(nodeWave).push(node);
    });
    [...waveGroups.entries()].sort(([a], [b]) => a - b).forEach(([nodeWave, waveNodes], columnIndex) => {
      waveNodes.forEach((node, index) => {
        const verticalOffset = (index - (waveNodes.length - 1) / 2) * 128;
        positions[node.id] = [155 + columnIndex * 290, 300 + verticalOffset];
      });
    });
  } else {
    // Use the curated layout when present (the Mac Runtime M-board); otherwise lay
    // milestones out in a spaced grid so ANY PRD (e.g. the P-pipeline board)
    // renders every milestone instead of stacking them at one fallback point.
    const overviewColumns = 4;
    nodes.forEach((node, index) => {
      if (overviewPositions[node.id]) {
        positions[node.id] = overviewPositions[node.id];
      } else {
        const row = Math.floor(index / overviewColumns);
        const column = index % overviewColumns;
        positions[node.id] = [200 + column * 400, 170 + row * 260];
      }
    });
  }

  const taskWaves = taskView
    ? new Set(nodes.map(node => displayWaves[node.id] ?? 1)).size
    : 0;
  const maxWaveSize = taskView
    ? Math.max(1, ...Object.values(nodes.reduce((counts, node) => {
      const nodeWave = displayWaves[node.id] ?? 1;
      counts[nodeWave] = (counts[nodeWave] || 0) + 1;
      return counts;
    }, {})))
    : 0;
  const contentWidth = taskView ? Math.max(1000, 310 + (taskWaves - 1) * 290) : 1745;
  const contentHeight = taskView ? Math.max(600, 220 + maxWaveSize * 128) : 760;
  svg.setAttribute("viewBox", `0 0 ${contentWidth} ${contentHeight}`);
  svg.style.width = `${Math.max(1000, contentWidth)}px`;
  svg.style.height = `${contentHeight}px`;

  const edgeLayer = svgElement("g");
  edges.forEach(edge => {
    if (!positions[edge.from] || !positions[edge.to]) return;
    const path = edgePath(positions[edge.from], positions[edge.to], width, taskView);
    edgeLayer.append(svgElement("path", { d: path, class: "edge" }));
    edgeLayer.append(svgElement("path", { d: path, class: "edge-flow" }));
  });
  svg.append(edgeLayer);

  nodes.forEach(node => {
    const [x, y] = positions[node.id];
    const assignment = taskView ? node.assignment : null;
    const agentLabel = assignment?.agent_label || assignment?.agent_id || "";
    const item = svgElement("g", {
      class: `node ${node.status}${state.selected?.id === node.id ? " selected" : ""}`,
      transform: `translate(${x},${y})`, tabindex: "0", role: "button",
      "aria-label": `${node.id}, ${node.title}, ${statusNames[node.status]}${agentLabel ? `, agent ${agentLabel}` : ""}`
    });
    item.append(svgElement("rect", { class: "node-aura", x: -width / 2 - 7, y: -height / 2 - 7, width: width + 14, height: height + 14, rx: 8 }));
    item.append(svgElement("rect", { class: "node-body", x: -width / 2, y: -height / 2, width, height, rx: 4 }));
    item.append(svgElement("rect", { class: "rail", x: -width / 2, y: -height / 2, width: 3, height, rx: 2 }));
    item.append(svgElement("circle", { class: "port", cx: -width / 2, cy: 0, r: 4 }));
    item.append(svgElement("circle", { class: "port", cx: width / 2, cy: 0, r: 4 }));

    const idText = svgElement("text", { class: "id-label", x: -width / 2 + 17, y: -height / 2 + 22 });
    idText.textContent = node.id;
    item.append(idText);
    if (assignment) {
      const badgeLabel = truncateLabel(agentLabel);
      const badgeWidth = Math.max(48, badgeLabel.length * 6.3 + 19);
      const badge = svgElement("g", {
        class: `agent-badge ${assignment.state || "checked_out"}`,
        transform: `translate(${width / 2 - 10},${-height / 2 + 16})`
      });
      const title = svgElement("title");
      title.textContent = `${agentLabel} · ${(assignment.state || "checked_out").replace("_", " ")}`;
      badge.append(title);
      badge.append(svgElement("rect", { x: -badgeWidth, y: -11, width: badgeWidth, height: 20, rx: 10 }));
      badge.append(svgElement("circle", { cx: -badgeWidth + 10, cy: -1, r: 3 }));
      const badgeText = svgElement("text", { x: -8, y: 2.5, "text-anchor": "end" });
      badgeText.textContent = badgeLabel;
      badge.append(badgeText);
      item.append(badge);
    }
    splitTitle(node.title, taskView ? 30 : 23).forEach((line, index) => {
      const label = svgElement("text", { class: "title-label", x: -width / 2 + 17, y: -3 + index * 17 });
      label.textContent = line;
      item.append(label);
    });
    const meta = svgElement("text", { class: "meta-label", x: -width / 2 + 17, y: height / 2 - 13 });
    const executionLabel = node.execution
      ? `W${node.execution.wave} · ${String(node.execution.mode || "sequential").toUpperCase()} · `
      : "";
    meta.textContent = taskView ? `${executionLabel}${statusNames[node.status].toUpperCase()}` : `${node.completed} / ${node.total} VERIFIED`;
    item.append(meta);
    const select = () => selectNode(node, taskView ? "task" : "milestone");
    item.addEventListener("click", select);
    item.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") select(); });
    svg.append(item);
  });
}

function selectNode(node, kind) {
  state.selected = { ...node, kind };
  document.getElementById("inspector-empty").classList.add("hidden");
  document.getElementById("inspector-content").classList.remove("hidden");
  const pill = document.getElementById("node-status");
  pill.className = `status-pill ${node.status}`;
  pill.textContent = statusNames[node.status];
  document.getElementById("node-kind").textContent = kind === "milestone" ? "EXECUTION MILESTONE" : (node.kind || "TASK").toUpperCase();
  document.getElementById("node-title").textContent = node.title;
  document.getElementById("node-id").textContent = node.id;
  document.getElementById("node-source").textContent = `${state.data.source} · line ${node.line}`;
  document.getElementById("node-gate").textContent = node.gate || (kind === "milestone" ? "Open milestone to inspect gate criteria" : "Inherited from milestone");
  const assignmentWrap = document.getElementById("node-assignment-wrap");
  if (kind === "task" && node.assignment) {
    const assignment = node.assignment;
    assignmentWrap.classList.remove("hidden");
    document.getElementById("node-agent").textContent = assignment.agent_label || assignment.agent_id;
    const activity = (assignment.state || "checked_out").replace("_", " ");
    const at = assignment.completed_at || assignment.released_at || assignment.checked_out_at;
    document.getElementById("node-assignment").textContent = at ? `${activity} · ${new Date(at).toLocaleString()}` : activity;
  } else {
    assignmentWrap.classList.add("hidden");
  }
  const executionWrap = document.getElementById("node-execution-wrap");
  if (kind === "task" && node.execution) {
    executionWrap.classList.remove("hidden");
    const group = node.execution.parallel_group ? ` · ${node.execution.parallel_group}` : "";
    document.getElementById("node-execution").textContent =
      `Wave ${node.execution.wave} · ${node.execution.mode}${group}`;
  } else {
    executionWrap.classList.add("hidden");
  }
  const instructionWrap = document.getElementById("node-instruction-wrap");
  if (kind === "task" && node.instruction) {
    instructionWrap.classList.remove("hidden");
    document.getElementById("node-instruction").textContent = node.instruction;
  } else {
    instructionWrap.classList.add("hidden");
  }
  const progressWrap = document.getElementById("node-progress-wrap");
  const open = document.getElementById("open-milestone");
  if (kind === "milestone") {
    progressWrap.classList.remove("hidden");
    document.getElementById("node-progress-label").textContent = "VERIFIED PROGRESS";
    document.getElementById("node-progress-count").textContent = `${node.completed} / ${node.total}`;
    document.getElementById("node-progress").style.width = `${node.progress}%`;
    open.classList.remove("hidden");
    open.onclick = () => openMilestone(node.id);
  } else {
    progressWrap.classList.add("hidden");
    open.classList.add("hidden");
  }
  renderGraph();
}

function openMilestone(id) {
  const group = state.data.groups.find(item => item.id === id);
  state.view = id;
  state.selected = null;
  document.getElementById("graph-kicker").textContent = `${id} · EXECUTABLE CHECKLIST`;
  document.getElementById("graph-title").textContent = group.title;
  document.getElementById("back").classList.remove("hidden");
  resetInspector();
  renderGraph();
  document.getElementById("graph-stage").scrollTo({ left: 0, top: 0, behavior: "smooth" });
}

function showOverview() {
  state.view = "overview";
  state.selected = null;
  document.getElementById("graph-kicker").textContent = "COMPILED EXECUTION PLAN";
  document.getElementById("graph-title").textContent = "Runtime implementation";
  document.getElementById("back").classList.add("hidden");
  resetInspector();
  renderGraph();
}

function resetInspector() {
  document.getElementById("inspector-empty").classList.remove("hidden");
  document.getElementById("inspector-content").classList.add("hidden");
}

function renderRunControl() {
  const control = state.data?.run_control;
  const button = document.getElementById("pause-build");
  const live = document.getElementById("live-state");
  if (!control?.available) {
    button.classList.add("hidden");
    return;
  }
  button.classList.remove("hidden");
  const halted = control.phase === "halted";
  button.disabled = halted || state.pausing;
  button.textContent = halted
    ? "✓ Build Paused"
    : state.pausing
      ? "Pausing…"
      : "Ⅱ Pause Build";
  button.classList.toggle("paused", halted);
  live.classList.toggle("paused", halted);
  live.lastChild.textContent = halted ? " BUILD PAUSED" : " BUILD RUNNING";
}

function formatTokens(value) {
  const amount = Number.isFinite(Number(value)) ? Math.max(0, Number(value)) : 0;
  if (amount >= 1_000_000_000) return `${(amount / 1_000_000_000).toFixed(amount >= 10_000_000_000 ? 0 : 1)}B`;
  if (amount >= 1_000_000) return `${(amount / 1_000_000).toFixed(amount >= 10_000_000 ? 0 : 1)}M`;
  if (amount >= 1_000) return `${(amount / 1_000).toFixed(amount >= 10_000 ? 0 : 1)}K`;
  return Math.round(amount).toLocaleString();
}

function renderEfficiency() {
  const efficiency = state.data?.efficiency;
  const build = efficiency?.build || {};
  const episodes = Array.isArray(efficiency?.episodes) ? efficiency.episodes : [];
  const estimated = Number(build.gross_estimated_tokens_avoided || 0);
  const adjusted = Number(build.confidence_adjusted_tokens_avoided || 0);
  const realized = Number(build.realized_tokens_saved || 0);
  const counter = document.getElementById("efficiency-counter");
  document.getElementById("efficiency-summary").textContent = `Estimated ${formatTokens(estimated)} tokens saved`;
  document.getElementById("efficiency-adjusted").textContent = `${formatTokens(adjusted)} tokens`;
  document.getElementById("efficiency-realized").textContent = `${formatTokens(realized)} tokens`;
  document.getElementById("efficiency-episodes").textContent = String(episodes.length);
  document.getElementById("efficiency-confidence").textContent =
    estimated > 0 ? `${Math.round((adjusted / estimated) * 100)}%` : "—";
  const status = document.getElementById("efficiency-status");
  const basis = document.getElementById("efficiency-basis");
  if (!efficiency) {
    status.textContent = "Efficiency tracking is awaiting canonical build data.";
    basis.textContent = "No savings are claimed until Fractal records an auditable envelope.";
  } else if (episodes.length === 0) {
    status.textContent = "Efficiency tracking is active. No avoidable work has been detected yet.";
    basis.textContent = "Estimated and realized savings remain zero until an intervention is recorded.";
  } else {
    const primary = episodes.reduce((best, episode) =>
      Number(episode.estimated_tokens_avoided || 0) > Number(best.estimated_tokens_avoided || 0)
        ? episode : best, episodes[0]);
    const affected = Number(primary.affected_count || primary.affected_node_ids?.length || 0);
    status.textContent = `${String(primary.waste_type || "efficiency").replaceAll("_", " ")} contained across ${affected} node${affected === 1 ? "" : "s"}.`;
    basis.textContent = primary.estimation_basis || "Estimate recorded from the active execution graph.";
  }
  if (estimated > state.estimatedSaved) {
    counter.classList.remove("pulse");
    requestAnimationFrame(() => counter.classList.add("pulse"));
  }
  state.estimatedSaved = estimated;
}

async function pauseBuild() {
  const control = state.data?.run_control;
  if (!control?.available || control.phase === "halted" || state.pausing) return;
  if (!window.confirm("Pause this build after preserving its completed graph waves?")) return;
  state.pausing = true;
  renderRunControl();
  try {
    const response = await fetch("/api/run/pause", {
      method: "POST",
      headers: { "X-Fractal-Control-Token": control.token }
    });
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || `Pause returned ${response.status}`);
    await loadGraph();
  } catch (error) {
    window.alert(`Fractal could not pause this build: ${error.message}`);
  } finally {
    state.pausing = false;
    renderRunControl();
  }
}

async function loadGraph() {
  const button = document.getElementById("refresh");
  button.disabled = true;
  try {
    const response = await fetch(`/api/graph?t=${Date.now()}`);
    if (!response.ok) throw new Error(`Graph API returned ${response.status}`);
    state.data = await response.json();
    renderRunControl();
    renderEfficiency();
    const totals = state.data.totals;
    document.getElementById("percent").textContent = `${totals.percent}%`;
    document.getElementById("completed").textContent = totals.complete;
    document.getElementById("active").textContent = totals.active;
    document.getElementById("remaining").textContent = totals.incomplete;
    document.getElementById("source-label").textContent = `SOURCE · ${state.data.source} · ${new Date(state.data.source_mtime).toLocaleString()}`;
    const development = state.data.development;
    const note = document.getElementById("source-label");
    if (development && development.visible) {
      const reshaping = development.reshaping_count || 0;
      note.textContent += ` · DEVELOPMENT · ${development.steps.length} step(s), ${reshaping} grow/repair`;
    }
    // A per-graph board has a single group whose "overview" is just one node;
    // open straight to its graph nodes on first load (still navigable after).
    if (!state.initialized) {
      state.initialized = true;
      if (Array.isArray(state.data.groups) && state.data.groups.length === 1) {
        state.view = state.data.groups[0].id;
      }
    }
    if (state.view !== "overview" && !state.data.groups.some(group => group.id === state.view)) state.view = "overview";
    renderGraph();
  } catch (error) {
    document.getElementById("graph-title").textContent = "Graph unavailable";
    document.getElementById("source-label").textContent = error.message;
  } finally {
    button.disabled = false;
  }
}

document.getElementById("refresh").addEventListener("click", loadGraph);
document.getElementById("back").addEventListener("click", showOverview);
document.getElementById("pause-build").addEventListener("click", pauseBuild);
loadGraph();
setInterval(loadGraph, 2000);
