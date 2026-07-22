"use strict";

const NS = "http://www.w3.org/2000/svg";
const state = { data: null, view: "overview", selected: null };
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
  const height = taskView ? 80 : 100;
  const columns = taskView ? 4 : 0;
  const positions = {};

  if (taskView) {
    nodes.forEach((node, index) => {
      const row = Math.floor(index / columns);
      const logicalColumn = index % columns;
      const column = row % 2 ? columns - logicalColumn - 1 : logicalColumn;
      positions[node.id] = [155 + column * 290, 105 + row * 130];
    });
  } else {
    nodes.forEach(node => { positions[node.id] = overviewPositions[node.id] || [100, 100]; });
  }

  const contentWidth = taskView ? 1180 : 1745;
  const rows = taskView ? Math.ceil(nodes.length / columns) : 6;
  const contentHeight = taskView ? Math.max(600, 145 + rows * 130) : 760;
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
    const item = svgElement("g", {
      class: `node ${node.status}${state.selected?.id === node.id ? " selected" : ""}`,
      transform: `translate(${x},${y})`, tabindex: "0", role: "button",
      "aria-label": `${node.id}, ${node.title}, ${statusNames[node.status]}`
    });
    item.append(svgElement("rect", { class: "node-aura", x: -width / 2 - 7, y: -height / 2 - 7, width: width + 14, height: height + 14, rx: 8 }));
    item.append(svgElement("rect", { class: "node-body", x: -width / 2, y: -height / 2, width, height, rx: 4 }));
    item.append(svgElement("rect", { class: "rail", x: -width / 2, y: -height / 2, width: 3, height, rx: 2 }));
    item.append(svgElement("circle", { class: "port", cx: -width / 2, cy: 0, r: 4 }));
    item.append(svgElement("circle", { class: "port", cx: width / 2, cy: 0, r: 4 }));

    const idText = svgElement("text", { class: "id-label", x: -width / 2 + 17, y: -height / 2 + 22 });
    idText.textContent = node.id;
    item.append(idText);
    splitTitle(node.title, taskView ? 30 : 23).forEach((line, index) => {
      const label = svgElement("text", { class: "title-label", x: -width / 2 + 17, y: -3 + index * 17 });
      label.textContent = line;
      item.append(label);
    });
    const meta = svgElement("text", { class: "meta-label", x: -width / 2 + 17, y: height / 2 - 13 });
    meta.textContent = taskView ? statusNames[node.status].toUpperCase() : `${node.completed} / ${node.total} VERIFIED`;
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

async function loadGraph() {
  const button = document.getElementById("refresh");
  button.disabled = true;
  try {
    const response = await fetch(`/api/graph?t=${Date.now()}`);
    if (!response.ok) throw new Error(`Graph API returned ${response.status}`);
    state.data = await response.json();
    const totals = state.data.totals;
    document.getElementById("percent").textContent = `${totals.percent}%`;
    document.getElementById("completed").textContent = totals.complete;
    document.getElementById("active").textContent = totals.active;
    document.getElementById("remaining").textContent = totals.incomplete;
    document.getElementById("source-label").textContent = `SOURCE · ${state.data.source} · ${new Date(state.data.source_mtime).toLocaleString()}`;
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
loadGraph();
setInterval(loadGraph, 30000);
