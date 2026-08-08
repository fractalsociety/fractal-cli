"use strict";

const NS = "http://www.w3.org/2000/svg";
const state = {
  data: null, view: "overview", selected: null, initialized: false,
  pausing: false, estimatedSaved: 0,
  liveSnapshot: null, announcementTimer: null, lastAnnouncement: "",
  failure: {
    data: null, loading: false, error: null, open: false, selected: "",
    filters: { q: "", state: [], code: "", component: "", lesson: "" }
  }
};
const statusNames = { complete: "Complete", active: "In progress", incomplete: "Incomplete" };
let masterBrowser = null;
let threeGraph = null;

function runtimeNodeState(node) {
  const assignment = node?.assignment || null;
  const evidence = node?.evidence || null;
  const status = assignment?.state === "completed" ? "complete"
    : assignment?.state === "checked_out" ? "active" : String(node?.status || "incomplete");
  return {
    status,
    assignment: assignment ? {
      agent_id: assignment.agent_id || "", agent_label: assignment.agent_label || "", state: assignment.state || "",
      checked_out_at: assignment.checked_out_at || "", completed_at: assignment.completed_at || "", released_at: assignment.released_at || ""
    } : null,
    evidence: evidence ? JSON.stringify(evidence) : ""
  };
}

/* Compare only server-reported facts.  Initial snapshots are quiet so opening
 * a graph never invents a completion animation; every later transition is
 * bounded and deterministic for the two-second poll cadence. */
function classifyTransitions(previous, next) {
  const prior = new Map((previous?.nodes || []).map(node => [node.id, runtimeNodeState(node)]));
  const transitions = [];
  (next?.nodes || []).forEach(node => {
    const before = prior.get(node.id);
    if (!before) return;
    const after = runtimeNodeState(node);
    if (before.status !== "active" && after.status === "active") transitions.push({ id: node.id, type: "became_active", detail: node.objective || node.title });
    if (before.status !== "complete" && after.status === "complete") transitions.push({ id: node.id, type: "completed", detail: node.objective || node.title });
    if (before.status === "active" && after.assignment?.state === "released") transitions.push({ id: node.id, type: "released", detail: node.objective || node.title });
    const beforeAssignment = JSON.stringify(before.assignment || null);
    const afterAssignment = JSON.stringify(after.assignment || null);
    if (beforeAssignment !== afterAssignment && !((before.status === "active" && after.status === "complete") || (before.status === "active" && after.assignment?.state === "released"))) {
      transitions.push({ id: node.id, type: "assignment_changed", detail: after.assignment?.agent_label || after.assignment?.agent_id || "assignment changed" });
    }
    if (before.evidence !== after.evidence) {
      transitions.push({ id: node.id, type: after.evidence.includes('"passed":false') ? "failed_verification" : "evidence_updated", detail: node.evidence?.outcome || "evidence updated" });
    }
  });
  return transitions.slice(0, 64);
}

// Expose the pure classifier for offline diagnostics and controller tests while
// keeping snapshot ownership in this polling layer.
window.FractalExecutionTransitions = { classifyTransitions };

function activeRunModel() {
  if (!window.FractalThreeGraph || !state.data) return null;
  return window.FractalThreeGraph.normalizeGraphPayload(state.data, state.view);
}

function renderLiveHud(model, transitions = []) {
  const hud = document.getElementById("live-work-hud");
  if (!hud || !model) return;
  const active = (model.nodes || []).filter(node => node.status === "active");
  const progress = model.execution?.progress;
  const phase = String(model.execution?.phase || "planning").replaceAll("_", " ");
  const settled = model.nodes.length > 0 && active.length === 0 && model.nodes.every(node => node.status === "complete");
  const agents = settled ? [] : [...new Set(active.map(node => node.assignment?.agent_label || node.assignment?.agent_id).filter(Boolean))];
  const selected = state.selected ? model.nodes.find(node => node.id === state.selected.id) : null;
  const objective = settled ? "Run complete — all nodes settled." : selected?.objective || active[0]?.objective || progress?.message || "Waiting for the next eligible node.";
  const why = settled ? "All nodes completed; no agent is active." : selected?.why?.reason || active[0]?.why?.reason || (progress?.agent_label ? `Assigned to ${progress.agent_label}.` : "The coordinator is evaluating dependencies.");
  document.getElementById("live-work-phase").textContent = phase.toUpperCase();
  document.getElementById("live-work-agents").textContent = agents.length ? agents.join(" · ") : "No active agent";
  document.getElementById("live-work-objective").textContent = objective;
  document.getElementById("live-work-why").textContent = why;
  hud.classList.toggle("calm", settled || (!active.length && phase === "completed"));
  if (!transitions.length) return;
  const announcement = transitions.map(item => {
    const label = item.detail || item.id;
    if (item.type === "completed") return `${label} completed.`;
    if (item.type === "released") return `${label} was released for review.`;
    if (item.type === "failed_verification") return `${label} has failed verification.`;
    if (item.type === "became_active") return `${label} is now active.`;
    return `${label} changed.`;
  }).join(" ");
  if (announcement === state.lastAnnouncement) return;
  state.lastAnnouncement = announcement;
  const live = document.getElementById("transition-announcements");
  if (!live) return;
  clearTimeout(state.announcementTimer);
  state.announcementTimer = setTimeout(() => { live.textContent = announcement; }, 120);
}

function disposeThreeGraph() {
  if (!threeGraph) return;
  threeGraph.destroy();
  threeGraph = null;
}

function ensureThreeGraph() {
  if (threeGraph || !window.FractalThreeGraph || queryMode() === "master") return threeGraph;
  const mount = document.getElementById("graph-3d");
  if (!mount) return null;
  threeGraph = window.FractalThreeGraph.createThreeGraph({
    mount,
    accessibleList: document.getElementById("graph-accessible-list"),
    fallbackSvg: document.getElementById("graph"),
    onSelect: (id, kind) => {
      const model = state.view === "overview" ? state.data?.overview : state.data?.groups?.find(group => group.id === state.view);
      const nodes = model?.tasks || model?.nodes || [];
      const node = nodes.find(item => item.id === id);
      if (node) selectNode(node, kind || (state.view === "overview" ? "milestone" : "task"));
    },
    onOpenMilestone: openMilestone
  });
  return threeGraph;
}

function queryMode() {
  return new URLSearchParams(window.location.search).get("mode") === "master" ? "master" : "individual";
}

function setBoardMode(mode) {
  document.body.classList.toggle("master-active", mode === "master");
  document.getElementById("master-browser").classList.toggle("hidden", mode !== "master");
  /* Pause is an execution control and must never appear to apply to the
   * read-only estate view. Individual mode lets renderRunControl restore it
   * after the project payload arrives. */
  if (mode === "master") document.getElementById("pause-build").classList.add("hidden");
  renderSharedModeToggle();
}

function renderSharedModeToggle() {
  const host = document.getElementById("view-mode-toggle");
  if (!host || !window.FractalMasterGraph) return;
  FractalMasterGraph.renderModeToggle(document, host, queryMode(), mode => {
    if (mode === "master") switchToMaster();
    else switchToIndividual();
  });
}

function individualUrl() {
  const params = new URLSearchParams(window.location.search);
  params.delete("mode");
  params.delete("view");
  params.delete("sel");
  params.delete("panel");
  return `${window.location.pathname}${params.toString() ? `?${params}` : ""}`;
}

function switchToIndividual(push = true) {
  if (push) history.pushState(null, "", individualUrl());
  setBoardMode("individual");
  loadGraph();
}

function switchToMaster(push = true) {
  const params = new URLSearchParams(window.location.search);
  params.set("mode", "master");
  if (push) history.pushState(null, "", `${window.location.pathname}?${params}`);
  /* The master browser owns its own SVG scene. Release the decorative WebGL
   * controller while it is hidden so its RAF loop and canvas cannot outlive
   * the individual graph view. It will be recreated on the next visit. */
  disposeThreeGraph();
  setBoardMode("master");
  /* Rebuild the modular controller when entering master so URL state (q,
   * filters, selection, and view) is authoritative after an individual visit. */
  if (masterBrowser) {
    document.getElementById("master-browser").replaceChildren();
    masterBrowser = null;
  }
  masterBrowser = FractalMasterGraph.createMasterGraphBrowser({
    root: document.getElementById("master-browser"),
    fetchImpl: window.fetch.bind(window),
    history: window.history,
    getSearch: () => window.location.search,
    getViewportWidth: () => window.innerWidth,
    sharedModeToggle: true,
    onModeChange: mode => {
      setBoardMode(mode);
      if (mode === "individual") loadGraph();
    }
  });
  masterBrowser.init().finally(renderMasterMetrics);
}

function renderMasterMetrics() {
  if (!masterBrowser) return;
  const view = masterBrowser.getState().master;
  if (!view) return;
  const host = document.querySelector("#master-browser .mg-region-metrics");
  if (!host) return;
  host.replaceChildren();
  FractalMasterGraph.masterHeroMetrics(view).forEach(metric => {
    const article = document.createElement("article");
    const value = document.createElement("span");
    value.className = "mg-metric-value";
    value.textContent = metric.value;
    const label = document.createElement("small");
    label.textContent = metric.label;
    article.append(value, label);
    host.append(article);
  });
}

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

function syncFailureFromUrl() {
  if (!window.FractalMasterGraph?.failureQueryState) return;
  const query = FractalMasterGraph.failureQueryState(window.location.search);
  state.failure.open = query.open;
  state.failure.selected = query.selected;
  state.failure.filters = {
    q: query.query || "", state: query.state || [], code: query.code || "",
    component: query.component || "", lesson: query.lesson || ""
  };
}

function persistFailureUrl(push = true) {
  if (!window.FractalMasterGraph?.parseQueryState) return;
  const query = FractalMasterGraph.parseQueryState(window.location.search);
  query.failurePanel = Boolean(state.failure.open);
  query.failureSel = state.failure.selected || "";
  query.failureQuery = state.failure.filters.q || "";
  query.failureState = (state.failure.filters.state || []).slice();
  query.failureCode = state.failure.filters.code || "";
  query.failureComponent = state.failure.filters.component || "";
  query.failureLesson = state.failure.filters.lesson || "";
  const text = FractalMasterGraph.serializeQueryState(query);
  const target = `${window.location.pathname}${text || "?"}`;
  if (push) history.pushState(null, "", target);
  else history.replaceState(null, "", target);
}

function failureRecords() {
  return FractalMasterGraph.failureRecords(state.failure.data);
}

function failureCountForNode(nodeId) {
  if (!nodeId) return 0;
  return failureRecords().filter(record => record.node_id === nodeId).length;
}

function failureSummaryLabel(summary) {
  const unresolved = Number(summary?.unresolved || 0);
  const total = Number(summary?.total || unresolved || 0);
  if (!total) return "No failures";
  return `${unresolved} open · ${total} total`;
}

function appendFailureEvidence(container, refs) {
  const list = Array.isArray(refs) ? refs : [];
  if (!list.length) {
    const empty = document.createElement("small");
    empty.className = "failure-empty-copy";
    empty.textContent = "No evidence hash recorded.";
    container.append(empty);
    return;
  }
  list.slice(0, 20).forEach(ref => {
    const row = document.createElement("code");
    row.className = "failure-evidence-hash";
    const value = ref.sha256 || ref.legacy_ref || "unidentified evidence";
    row.textContent = ref.sha256 ? `sha256:${String(value).replace(/^sha256:/, "")}` : String(value);
    container.append(row);
  });
}

function renderFailureHistory() {
  const section = document.getElementById("failure-history");
  if (!section) return;
  const toggle = document.getElementById("failure-history-toggle");
  const panel = document.getElementById("failure-history-panel");
  const list = document.getElementById("failure-history-list");
  const detail = document.getElementById("failure-history-detail");
  if (!toggle || !panel || !list || !detail) return;
  const records = failureRecords();
  const summary = state.failure.data?.summary || {};
  toggle.textContent = `Failure History · ${failureSummaryLabel(summary)}`;
  toggle.setAttribute("aria-expanded", state.failure.open ? "true" : "false");
  panel.hidden = !state.failure.open;
  section.classList.toggle("failure-history-open", state.failure.open);
  list.replaceChildren();
  detail.replaceChildren();
  if (!state.failure.open) return;
  if (state.failure.loading) {
    list.append(Object.assign(document.createElement("p"), { className: "failure-state-copy", textContent: "Loading failure history…" }));
    return;
  }
  if (state.failure.error) {
    const message = document.createElement("p");
    message.className = "failure-state-copy failure-state-error";
    message.textContent = `Failure history unavailable: ${state.failure.error}`;
    list.append(message);
    const retry = document.createElement("button");
    retry.type = "button"; retry.className = "failure-retry"; retry.textContent = "Retry";
    retry.addEventListener("click", () => loadFailureGraph(true));
    list.append(retry);
    return;
  }

  const columns = panel.querySelector(".failure-history-columns");
  const controls = document.createElement("div");
  controls.className = "failure-filters";
  const search = document.createElement("input");
  search.type = "search"; search.className = "failure-search"; search.placeholder = "Search failure history";
  search.setAttribute("aria-label", "Search failure history"); search.value = state.failure.filters.q || "";
  search.addEventListener("input", () => {
    state.failure.filters.q = search.value.slice(0, 200); state.failure.selected = ""; persistFailureUrl(true); renderFailureHistory();
  });
  controls.append(search);
  const selectFilter = (label, key, values) => {
    const select = document.createElement("select");
    select.className = "failure-filter-select"; select.setAttribute("aria-label", `Filter by ${label}`);
    const all = document.createElement("option"); all.value = ""; all.textContent = `All ${label}`; select.append(all);
    values.forEach(value => { const option = document.createElement("option"); option.value = value; option.textContent = value; select.append(option); });
    select.value = state.failure.filters[key] || "";
    select.addEventListener("change", () => { state.failure.filters[key] = select.value; state.failure.selected = ""; persistFailureUrl(true); renderFailureHistory(); });
    controls.append(select);
  };
  selectFilter("codes", "code", FractalMasterGraph.failureFieldValues(records, "failure_code"));
  selectFilter("components", "component", FractalMasterGraph.failureFieldValues(records, "component"));
  selectFilter("lessons", "lesson", FractalMasterGraph.failureLessons(state.failure.data).map(item => item.id || item.summary).filter(Boolean).sort());
  const stateFilters = document.createElement("div"); stateFilters.className = "failure-state-filters"; stateFilters.setAttribute("role", "group"); stateFilters.setAttribute("aria-label", "Filter by failure state");
  FractalMasterGraph.FAILURE_STATES.forEach(token => {
    const button = document.createElement("button"); button.type = "button"; button.className = "failure-state-filter";
    button.textContent = token; button.setAttribute("aria-pressed", state.failure.filters.state.includes(token) ? "true" : "false");
    if (state.failure.filters.state.includes(token)) button.classList.add("active");
    button.addEventListener("click", () => {
      const current = state.failure.filters.state.filter(value => value !== token);
      if (!state.failure.filters.state.includes(token)) current.push(token);
      state.failure.filters.state = current; state.failure.selected = ""; persistFailureUrl(true); renderFailureHistory();
    });
    stateFilters.append(button);
  });
  controls.append(stateFilters);
  if (columns) panel.replaceChildren(controls, columns);

  const filtered = FractalMasterGraph.filterFailureRecords(records, state.failure.filters, state.failure.data);
  const bounded = FractalMasterGraph.boundedFailureRecords(filtered, 300);
  if (!bounded.records.length) {
    const empty = document.createElement("p"); empty.className = "failure-state-copy";
    empty.textContent = records.length ? "No failure history matches the active filters." : "No failures recorded for this project.";
    list.append(empty); return;
  }
  const selected = bounded.records.find(record => record.id === state.failure.selected) || bounded.records[0];
  if (selected.id !== state.failure.selected) { state.failure.selected = selected.id; persistFailureUrl(false); }
  bounded.records.forEach(record => {
    const item = document.createElement("button"); item.type = "button"; item.className = "failure-record-row";
    item.classList.toggle("selected", record.id === selected.id);
    item.setAttribute("role", "option"); item.setAttribute("aria-selected", record.id === selected.id ? "true" : "false");
    item.innerHTML = `<span class="failure-record-state state-${record.state || "unresolved"}">${record.state || "unresolved"}</span><strong></strong><small></small>`;
    item.querySelector("strong").textContent = `${record.node_id || "node"} · ${record.failure_code || "failure"}`;
    item.querySelector("small").textContent = record.summary || "";
    item.addEventListener("click", () => { state.failure.selected = record.id; persistFailureUrl(true); renderFailureHistory(); });
    list.append(item);
  });
  if (bounded.hiddenCount) {
    const cap = document.createElement("p"); cap.className = "failure-cap-copy"; cap.textContent = `${bounded.hiddenCount} additional records hidden to keep this view responsive.`; list.append(cap);
  }
  renderFailureDetail(detail, selected);
}

function renderFailureDetail(container, record) {
  if (!record) return;
  const title = document.createElement("h3"); title.textContent = record.summary || record.id; container.append(title);
  const meta = document.createElement("p"); meta.className = "failure-detail-meta"; meta.textContent = `${record.id} · ${record.node_id || "node"} · attempt ${record.attempt || 1}`; container.append(meta);
  const fields = document.createElement("dl"); fields.className = "failure-detail-fields";
  [["State", record.state || "unresolved"], ["Code", record.failure_code || "—"], ["Component", record.component || "—"], ["Capability", record.capability || "—"]].forEach(([label, value]) => {
    const wrap = document.createElement("div"); const dt = document.createElement("dt"); dt.textContent = label; const dd = document.createElement("dd"); dd.textContent = value; wrap.append(dt, dd); fields.append(wrap);
  });
  container.append(fields);
  const timelineTitle = document.createElement("h4"); timelineTitle.textContent = "Timeline"; container.append(timelineTitle);
  const timeline = document.createElement("ol"); timeline.className = "failure-timeline";
  FractalMasterGraph.failureTimeline(record).forEach(entry => {
    const item = document.createElement("li"); item.className = `failure-timeline-${entry.kind}`;
    const heading = document.createElement("strong"); heading.textContent = `${entry.kind} · ${entry.outcome}`; item.append(heading);
    const copy = document.createElement("p"); copy.textContent = entry.summary || ""; item.append(copy);
    const evidence = document.createElement("div"); evidence.className = "failure-evidence"; appendFailureEvidence(evidence, entry.evidence); item.append(evidence); timeline.append(item);
  });
  container.append(timeline);
  const lessonTitle = document.createElement("h4"); lessonTitle.textContent = "Lesson applicability"; container.append(lessonTitle);
  const lessons = FractalMasterGraph.failureLessonsForRecord(record, state.failure.data);
  const lessonCopy = document.createElement("p"); lessonCopy.textContent = lessons.length ? lessons.map(lesson => `${lesson.summary || lesson.id} (${lesson.status || "proposed"})`).join(" · ") : "No explicitly applicable lesson recorded."; container.append(lessonCopy);
  const pathTitle = document.createElement("h4"); pathTitle.textContent = "Explicit causal path"; container.append(pathTitle);
  const explicitEdges = FractalMasterGraph.failureEdges(state.failure.data);
  const path = FractalMasterGraph.failurePath(explicitEdges, record.id, record.superseded_by || record.id);
  const incident = explicitEdges.filter(edge => edge.from === record.id || edge.to === record.id);
  const pathCopy = document.createElement("p");
  pathCopy.textContent = (path.length ? path : incident).length
    ? (path.length ? path : incident).map(edge => `${edge.from} —${edge.type || "related"}→ ${edge.to}`).join(" · ")
    : "No explicit causal edges recorded.";
  container.append(pathCopy);
  const evidenceTitle = document.createElement("h4"); evidenceTitle.textContent = "Resolution / supersession evidence"; container.append(evidenceTitle);
  const evidence = document.createElement("div"); evidence.className = "failure-evidence"; appendFailureEvidence(evidence, record.resolution?.evidence || record.evidence); container.append(evidence);
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
  // A single-group individual board opens directly in task view on first
  // load, so keep the architecture back control in sync with the rendered
  // view instead of relying only on the explicit milestone-open path.
  document.getElementById("back")?.classList.toggle("hidden", !taskView);
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
    const graphApi = window.FractalThreeGraph || {};
    const taskNumber = taskView && typeof graphApi.canonicalTaskNumber === "function"
      ? graphApi.canonicalTaskNumber(node)
      : null;
    const taskLabel = taskNumber ? `Task ${taskNumber}` : "Task number unavailable";
    const overview = typeof graphApi.oneLineOverview === "function"
      ? graphApi.oneLineOverview(node)
      : String(node.objective || node.instruction || node.title || `Task ${node.id} has no recorded purpose.`).replace(/\s+/g, " ").trim();
    const failureCount = taskView
      ? failureCountForNode(node.id)
      : (state.failure.data?.summary?.total || 0);
    const agentLabel = assignment?.agent_label || assignment?.agent_id || "";
    const whyReason = node.why?.reason
      || (node.why?.ready === true ? "Ready to work." : node.why?.ready === false
        ? `Blocked by ${(node.why?.blocked_by || []).join(", ") || "dependency"}.`
        : "Dependency explanation not recorded.");
    const objective = node.objective || node.title;
    const evidenceSummary = node.evidence?.verification?.passed === true ? "verified evidence"
      : node.evidence?.verification?.passed === false ? "verification failed" : node.evidence?.outcome || "no outcome recorded";
    const item = svgElement("g", {
      class: `node ${node.status}${assignment?.state === "released" ? " released" : ""}${state.selected?.id === node.id ? " selected" : ""}`,
      transform: `translate(${x},${y})`, tabindex: "0", role: "button",
      "aria-label": `${taskLabel}, ${node.id}, ${node.title}, ${statusNames[node.status]}, ${overview}, objective ${objective}, ${whyReason}, ${evidenceSummary}${agentLabel ? `, agent ${agentLabel}` : ""}`,
      "aria-selected": state.selected?.id === node.id ? "true" : "false",
      "aria-current": state.selected?.id === node.id ? "true" : "false"
    });
    item.append(svgElement("rect", { class: "node-aura", x: -width / 2 - 7, y: -height / 2 - 7, width: width + 14, height: height + 14, rx: 8 }));
    item.append(svgElement("rect", { class: "node-body", x: -width / 2, y: -height / 2, width, height, rx: 4 }));
    item.append(svgElement("rect", { class: "rail", x: -width / 2, y: -height / 2, width: 3, height, rx: 2 }));
    item.append(svgElement("circle", { class: "port", cx: -width / 2, cy: 0, r: 4 }));
    item.append(svgElement("circle", { class: "port", cx: width / 2, cy: 0, r: 4 }));

    const idText = svgElement("text", { class: "id-label", x: -width / 2 + 17, y: -height / 2 + 22 });
    idText.textContent = node.id;
    item.append(idText);
    if (taskView) {
      const numberText = svgElement("text", { class: "task-number-label", x: -width / 2 + 17, y: -height / 2 + 10 });
      numberText.textContent = taskLabel;
      item.append(numberText);
    }
    if (failureCount > 0) {
      const failureBadge = svgElement("g", {
        class: "failure-badge",
        transform: `translate(${width / 2 - 15},${height / 2 - 16})`,
        tabindex: "0", role: "button",
        "aria-label": `${failureCount} failure${failureCount === 1 ? "" : "s"} recorded`
      });
      failureBadge.append(svgElement("circle", { cx: 0, cy: 0, r: 10 }));
      const failureText = svgElement("text", { x: 0, y: 4, "text-anchor": "middle" });
      failureText.textContent = String(failureCount);
      failureBadge.append(failureText);
      failureBadge.addEventListener("click", event => {
        event.stopPropagation();
        state.failure.open = true;
        const first = failureRecords().find(record => !taskView || record.node_id === node.id);
        if (first) state.failure.selected = first.id;
        persistFailureUrl(true);
        renderFailureHistory();
      });
      failureBadge.addEventListener("keydown", event => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault(); failureBadge.click();
        }
      });
      item.append(failureBadge);
    }
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
  const three = ensureThreeGraph();
  if (three && state.data && window.FractalThreeGraph) {
    const normalized = window.FractalThreeGraph.normalizeGraphPayload(state.data, state.view);
    const transitions = state.liveSnapshot ? classifyTransitions(state.liveSnapshot, normalized) : [];
    normalized.transitions = transitions;
    state.liveSnapshot = normalized;
    renderLiveHud(normalized, transitions);
    three.update(normalized, state.selected?.id || null);
  }
}

function taskDetailFor(node) {
  const api = window.FractalThreeGraph;
  const model = state.view === "overview"
    ? state.data?.overview
    : state.data?.groups?.find(group => group.id === state.view);
  if (api?.buildTaskDetail) return api.buildTaskDetail(node, model || null);
  const overview = String(node?.objective || node?.instruction || node?.title || `Task ${node?.id || "unknown"} has no recorded purpose.`).replace(/\s+/g, " ").trim();
  return {
    taskNumber: null,
    overview: overview.length <= 180 ? overview : `${overview.slice(0, 179)}…`,
    purpose: node?.objective || "Purpose not recorded.",
    why: { ready: null, reason: "Dependency explanation not recorded.", blockedBy: [] },
    dependencies: Array.isArray(node?.depends_on) ? node.depends_on : [],
    execution: { wave: null, mode: null, parallelGroup: null }, capability: node?.capability || null,
    instruction: node?.instruction || null, expectedOutput: node?.expected_output || null,
    agent: node?.assignment ? { id: node.assignment.agent_id || "", label: node.assignment.agent_label || "", state: node.assignment.state || "" } : null,
    evidence: node?.evidence || {}, gate: node?.gate || null
  };
}

function selectNode(node, kind) {
  const detail = kind === "task" ? taskDetailFor(node) : null;
  state.selected = { ...node, kind };
  document.getElementById("inspector-empty").classList.add("hidden");
  document.getElementById("inspector-content").classList.remove("hidden");
  const pill = document.getElementById("node-status");
  pill.className = `status-pill ${node.status}`;
  pill.textContent = statusNames[node.status];
  document.getElementById("node-kind").textContent = kind === "milestone" ? "EXECUTION MILESTONE" : (node.kind || "TASK").toUpperCase();
  document.getElementById("node-title").textContent = node.title;
  document.getElementById("node-task-number").textContent = kind === "task"
    ? (detail.taskNumber ? `Task ${detail.taskNumber}` : "Task number unavailable")
    : "";
  document.getElementById("node-id").textContent = node.id;
  document.getElementById("node-overview").textContent = detail?.overview || node.objective || node.title;
  document.getElementById("node-source").textContent = `${state.data.source} · line ${node.line}`;
  document.getElementById("node-gate").textContent = detail?.gate || (kind === "milestone" ? "Open milestone to inspect gate criteria" : "Gate criteria not recorded.");
  const why = node.why || { ready: null, blocked_by: [], reason: "Dependency explanation not recorded." };
  const readiness = detail?.why || {
    ready: typeof why.ready === "boolean" ? why.ready : null,
    reason: why.reason || "Dependency explanation not recorded.",
    blockedBy: Array.isArray(why.blocked_by) ? why.blocked_by : []
  };
  document.getElementById("node-objective").textContent = detail?.purpose || node.objective || "Purpose not recorded.";
  const readinessSummary = readiness.ready === true
    ? `Ready · ${readiness.reason || "Dependency explanation not recorded."}`
    : readiness.ready === false
      ? `Blocked · ${readiness.reason || "Dependency explanation not recorded."}`
      : (readiness.reason || "Dependency explanation not recorded.");
  /* Keep the stable #node-why child in the inspector.  Updating the parent
   * #node-readiness.textContent would remove that child and make the next
   * selection throw when it tries to update the readiness copy again. */
  document.getElementById("node-why").textContent = readinessSummary;
  document.getElementById("node-readiness").dataset.ready = readiness.ready == null
    ? "unknown" : readiness.ready ? "true" : "false";
  document.getElementById("node-dependencies").textContent = detail
    ? (detail.dependencies.length ? detail.dependencies.join(" · ") : "No dependencies recorded.")
    : (Array.isArray(node.depends_on) && node.depends_on.length ? node.depends_on.join(" · ") : "No dependencies recorded.");
  const evidence = detail?.evidence || node.evidence || {};
  const verification = evidence.verification || {};
  const evidenceElement = document.getElementById("node-evidence");
  evidenceElement.className = verification.passed === false ? "evidence-failed" : verification.passed === true ? "evidence-passed" : "";
  const evidenceParts = [];
  if (evidence.outcome) evidenceParts.push(String(evidence.outcome).replaceAll("_", " "));
  if (verification.passed === true) evidenceParts.push("verification passed");
  else if (verification.passed === false) evidenceParts.push("verification failed");
  if (verification.evidence_refs?.length) evidenceParts.push(`${verification.evidence_refs.length} evidence ref${verification.evidence_refs.length === 1 ? "" : "s"}`);
  if (evidence.attempt_count) evidenceParts.push(`attempt ${evidence.attempt_count}`);
  evidenceElement.textContent = evidenceParts.length ? evidenceParts.join(" · ") : "No evidence recorded yet.";
  const assignmentForEvent = node.assignment || null;
  const eventTime = evidence.finished_at || evidence.started_at || assignmentForEvent?.completed_at || assignmentForEvent?.released_at || assignmentForEvent?.checked_out_at;
  const eventLabel = evidence.finished_at ? "Evidence finished" : evidence.started_at ? "Work started" : assignmentForEvent?.completed_at ? "Assignment completed" : assignmentForEvent?.released_at ? "Assignment released" : assignmentForEvent?.checked_out_at ? "Assignment checked out" : "No runtime event recorded";
  document.getElementById("node-last-event").textContent = eventTime ? `${eventLabel} · ${new Date(eventTime).toLocaleString()}` : eventLabel;
  const assignmentWrap = document.getElementById("node-assignment-wrap");
  if (kind === "task") {
    assignmentWrap.classList.remove("hidden");
    const assignment = detail?.agent;
    document.getElementById("node-agent").textContent = assignment
      ? (assignment.label || assignment.id || "No agent assigned.")
      : "No agent assigned.";
    const activity = assignment?.state ? assignment.state.replaceAll("_", " ") : "No agent assigned.";
    const at = assignmentForEvent?.completed_at || assignmentForEvent?.released_at || assignmentForEvent?.checked_out_at;
    document.getElementById("node-assignment").textContent = at ? `${activity} · ${new Date(at).toLocaleString()}` : activity;
  } else {
    assignmentWrap.classList.add("hidden");
  }
  const executionWrap = document.getElementById("node-execution-wrap");
  const capabilityWrap = document.getElementById("node-capability-wrap");
  const instructionWrap = document.getElementById("node-instruction-wrap");
  const expectedOutputWrap = document.getElementById("node-expected-output-wrap");
  if (kind === "task") {
    executionWrap.classList.remove("hidden");
    document.getElementById("node-execution-wave").textContent = detail?.execution.wave == null ? "Execution wave not recorded." : String(detail.execution.wave);
    document.getElementById("node-execution-mode").textContent = detail?.execution.mode || "Execution mode not recorded.";
    document.getElementById("node-execution-group").textContent = detail?.execution.parallelGroup || "Parallel group not recorded.";
    document.getElementById("node-execution").textContent = `${document.getElementById("node-execution-wave").textContent} · ${document.getElementById("node-execution-mode").textContent} · ${document.getElementById("node-execution-group").textContent}`;
    capabilityWrap.classList.remove("hidden");
    document.getElementById("node-capability").textContent = detail?.capability || "Capability not recorded.";
    instructionWrap.classList.remove("hidden");
    document.getElementById("node-instruction").textContent = detail?.instruction || "Instruction not recorded.";
    expectedOutputWrap.classList.remove("hidden");
    document.getElementById("node-expected-output").textContent = detail?.expectedOutput || "Expected output not recorded.";
  } else {
    executionWrap.classList.add("hidden");
    capabilityWrap.classList.add("hidden");
    instructionWrap.classList.add("hidden");
    expectedOutputWrap.classList.add("hidden");
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
  if (kind === "task") {
    const focusRef = detail?.taskNumber || node.id;
    threeGraph?.focus?.(focusRef);
  }
}

function openMilestone(id) {
  const group = state.data.groups.find(item => item.id === id);
  state.view = id;
  state.selected = null;
  state.liveSnapshot = null;
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
  state.liveSnapshot = null;
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

async function loadFailureGraph(bust = false) {
  if (queryMode() === "master") return;
  const project = new URLSearchParams(window.location.search).get("project") || "";
  state.failure.loading = !state.failure.data;
  state.failure.error = null;
  syncFailureFromUrl();
  renderFailureHistory();
  try {
    const api = FractalMasterGraph.createApiClient(window.fetch.bind(window), { now: () => Date.now() });
    state.failure.data = await api.failureGraph(project, bust);
    state.failure.loading = false;
    state.failure.error = null;
    const records = failureRecords();
    if (state.failure.selected && !records.some(record => record.id === state.failure.selected)) state.failure.selected = "";
    renderFailureHistory();
  } catch (error) {
    state.failure.loading = false;
    state.failure.error = error.message || "request failed";
    renderFailureHistory();
  }
}

async function loadGraph() {
  if (queryMode() === "master") return;
  const button = document.getElementById("refresh");
  button.disabled = true;
  try {
    const params = new URLSearchParams(window.location.search);
    const project = params.get("project");
    const response = await fetch(`/api/graph${project ? `?project=${encodeURIComponent(project)}&` : "?"}t=${Date.now()}`);
    if (!response.ok) throw new Error(`Graph API returned ${response.status}`);
    state.data = await response.json();
    syncFailureFromUrl();
    await loadFailureGraph(false);
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
    renderFailureHistory();
  } catch (error) {
    document.getElementById("graph-title").textContent = "Graph unavailable";
    document.getElementById("source-label").textContent = error.message;
  } finally {
    button.disabled = false;
  }
}

document.getElementById("refresh").addEventListener("click", () => {
  if (queryMode() === "master") {
    if (masterBrowser) masterBrowser.refresh();
  } else loadGraph();
});
document.getElementById("back").addEventListener("click", showOverview);
document.getElementById("pause-build").addEventListener("click", pauseBuild);
document.getElementById("failure-history-toggle")?.addEventListener("click", () => {
  state.failure.open = !state.failure.open;
  persistFailureUrl(true);
  renderFailureHistory();
  if (state.failure.open && !state.failure.data) loadFailureGraph(false);
});
document.getElementById("graph-reset-camera")?.addEventListener("click", () => threeGraph?.resetCamera());
document.getElementById("graph-list-toggle")?.addEventListener("click", event => {
  const list = document.getElementById("graph-accessible-list");
  if (!list) return;
  const open = list.classList.toggle("hidden");
  event.currentTarget.setAttribute("aria-expanded", String(!open));
});

window.addEventListener("popstate", () => {
  if (queryMode() === "master") {
    /* The modular controller intentionally receives an injected history object;
     * rebuild its mounted regions on browser Back/Forward so every query field
     * (search, filters, selection, panel) is restored from the URL. */
    if (masterBrowser) {
      document.getElementById("master-browser").replaceChildren();
      masterBrowser = null;
    }
    switchToMaster(false);
  }
  else switchToIndividual(false);
});

if (queryMode() === "master") switchToMaster();
else {
  syncFailureFromUrl();
  setBoardMode("individual");
  renderSharedModeToggle();
  renderFailureHistory();
  loadGraph();
}
setInterval(() => { if (queryMode() === "individual") loadGraph(); }, 2000);
