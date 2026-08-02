"use strict";
/*
 * master-graph.js — modular behaviors for the Individual / Master Graph Browser.
 *
 * Implements docs/master-graph-browser-contract.md ("Modular behaviors" wave,
 * contract §16): pure state helpers plus DOM renderers that only touch injected
 * roots. This file does not wire itself into index.html / app.js — the
 * integrate wave does that — and it never issues a mutation request.
 *
 * Everything is testable headlessly: fetch, document, history, location,
 * timers, and viewport width are all injectable.
 */
(function (globalScope) {

  var SVG_NS = "http://www.w3.org/2000/svg";

  /* ------------------------------------------------------------------ *
   * Vocabulary (contract §6, §7, §9, §14)
   * ------------------------------------------------------------------ */

  var MODES = ["individual", "master"];

  var PANELS = ["info", "evidence", "tests", "decisions", "diagnostics"];

  var STATUS_TOKENS = {
    individual: ["complete", "active", "incomplete"],
    master: [
      "verified", "implemented_unverified", "partial", "unknown",
      "available", "unavailable", "invalid", "missing"
    ]
  };

  var REL_TOKENS = [
    "depends_on", "uses_component", "derived_from", "forked_from",
    "supersedes", "shares_component", "related_to", "dep", "predecessor"
  ];

  var NODE_NAMESPACES = ["project", "component", "capability"];
  var EDGE_NAMESPACES = ["dep", "link"];

  var STATUS_LABELS = {
    complete: "Complete", active: "In progress", incomplete: "Incomplete",
    verified: "Verified", implemented_unverified: "Implemented, unverified",
    partial: "Partial", unknown: "Unknown", available: "Available",
    unavailable: "Unavailable", invalid: "Invalid catalog", missing: "Not audited"
  };

  /* Rendering budget (contract §14). Data model is never truncated — these
   * caps bound only what gets mounted in the DOM. */
  var RENDER_BUDGET = {
    maxSvgNodes: 300,
    maxSvgEdgePaths: 400,
    virtualizeRowsOver: 200,
    rowOverscan: 40,
    evidenceCap: 20,
    logExcerptCap: 1024,
    searchDebounceMs: 200,
    minAutoRefreshMs: 10000,
    narrowViewportPx: 560,
    maxQueryLength: 200
  };

  var ERROR_COPY = {
    not_found: { title: "Not found", message: "The requested resource is not on this board.", retryable: false },
    not_in_inventory: { title: "Project not in inventory", message: "This project key is not part of the frozen inventory. Pick a project from the switcher.", retryable: false },
    unavailable_project: { title: "Project unavailable", message: "The workspace for this project could not be read. No graph nodes were fabricated.", retryable: false },
    invalid_project: { title: "Invalid catalog", message: "This project's catalog failed validation. Diagnostics are shown instead of a partial parse.", retryable: false },
    read_only: { title: "Read-only surface", message: "This board surface does not accept writes.", retryable: false },
    compose_failed: { title: "Master composition failed", message: "The master view could not be composed. Individual mode is unaffected.", retryable: true },
    bad_request: { title: "Bad request", message: "The board rejected this request.", retryable: false },
    network: { title: "Graph unavailable", message: "The board did not respond. Check that the board server is running, then retry.", retryable: true }
  };

  /* ------------------------------------------------------------------ *
   * Small utilities
   * ------------------------------------------------------------------ */

  function normalizeText(value) {
    return String(value == null ? "" : value).toLowerCase().replace(/\s+/g, " ").trim();
  }

  function uniq(list) {
    var seen = {}, out = [];
    (list || []).forEach(function (item) {
      if (!seen[item]) { seen[item] = true; out.push(item); }
    });
    return out;
  }

  function cssEscape(value) {
    if (typeof CSS !== "undefined" && CSS.escape) return CSS.escape(value);
    return String(value).replace(/["\\\]\[#.:>+~*^$|()\s]/g, "\\$&");
  }

  function clampText(value, max) {
    var text = String(value == null ? "" : value);
    return text.length <= max ? text : text.slice(0, max - 1) + "…";
  }

  /* ------------------------------------------------------------------ *
   * URL query state (contract §6)
   * ------------------------------------------------------------------ */

  var QUERY_DEFAULTS = {
    mode: "individual", project: "", view: "", sel: "",
    q: "", status: [], rel: [], panel: "info"
  };

  function parseQueryState(search) {
    var params = new URLSearchParams(String(search == null ? "" : search));
    var state = {
      mode: QUERY_DEFAULTS.mode, project: "", view: "", sel: "",
      q: "", status: [], rel: [], panel: QUERY_DEFAULTS.panel
    };
    var mode = params.get("mode");
    if (MODES.indexOf(mode) !== -1) state.mode = mode;
    state.project = params.get("project") || "";
    state.view = params.get("view") || "";
    state.sel = params.get("sel") || "";
    state.q = String(params.get("q") || "").slice(0, RENDER_BUDGET.maxQueryLength);
    state.status = sanitizeStatusTokens((params.get("status") || "").split(","), state.mode);
    state.rel = sanitizeRelTokens((params.get("rel") || "").split(","));
    var panel = params.get("panel");
    if (PANELS.indexOf(panel) !== -1) state.panel = panel;
    // Unknown params are ignored (forward compatible); `t` cache-bust is
    // orthogonal and never round-trips through application state.
    return state;
  }

  function serializeQueryState(state) {
    var params = new URLSearchParams();
    if (state.mode && state.mode !== QUERY_DEFAULTS.mode) params.set("mode", state.mode);
    if (state.project) params.set("project", state.project);
    if (state.view) params.set("view", state.view);
    if (state.sel) params.set("sel", state.sel);
    if (state.q) params.set("q", String(state.q).slice(0, RENDER_BUDGET.maxQueryLength));
    if (state.status && state.status.length) params.set("status", state.status.join(","));
    if (state.rel && state.rel.length) params.set("rel", state.rel.join(","));
    if (state.panel && state.panel !== QUERY_DEFAULTS.panel) params.set("panel", state.panel);
    var text = params.toString();
    return text ? "?" + text : "";
  }

  /* pushState for user-driven changes, replaceState for poll-driven noise. */
  function applyQueryState(history, state, options) {
    if (!history) return "";
    var query = serializeQueryState(state);
    var url = query || (typeof location !== "undefined" && location.pathname) || "?";
    if (options && options.push) history.pushState(null, "", query || "?");
    else history.replaceState(null, "", query || "?");
    return url;
  }

  /* Default primary-stage view (contract §6.1, §13.1). */
  function effectiveView(state, viewportWidth) {
    if (state.view === "list" || state.view === "graph") return state.view;
    if (state.mode === "master") {
      return viewportWidth <= RENDER_BUDGET.narrowViewportPx ? "list" : "graph";
    }
    return state.view || "overview";
  }

  /* ------------------------------------------------------------------ *
   * Namespaced identity + selection model (contract §2.4, §2.5)
   * ------------------------------------------------------------------ */

  function parseNamespacedId(id) {
    var text = String(id == null ? "" : id);
    var split = text.indexOf(":");
    if (split <= 0) return null;
    var namespace = text.slice(0, split);
    if (NODE_NAMESPACES.indexOf(namespace) === -1 && EDGE_NAMESPACES.indexOf(namespace) === -1) return null;
    return { namespace: namespace, rest: text.slice(split + 1), id: text };
  }

  function classifySelection(sel, mode) {
    if (!sel) return { kind: "none" };
    var parsed = parseNamespacedId(sel);
    if (parsed) {
      if (parsed.namespace === "project") return { kind: "project", project_key: parsed.rest, id: parsed.id };
      if (parsed.namespace === "component" || parsed.namespace === "capability") {
        return { kind: "master_node", node_id: parsed.id };
      }
      return { kind: "master_edge", edge_id: parsed.id };
    }
    if (/^diag:\d+$/.test(sel)) return { kind: "diagnostic", diagnostic_index: Number(sel.slice(5)) };
    if (mode === "master") return { kind: "master_node", node_id: sel };
    return { kind: "task", task_id: sel };
  }

  /* ------------------------------------------------------------------ *
   * Debounced search (contract §7.1)
   * ------------------------------------------------------------------ */

  function createDebouncer(delayMs, timers) {
    var delay = Number.isFinite(delayMs) ? delayMs : RENDER_BUDGET.searchDebounceMs;
    var setT = (timers && timers.setTimeout) || setTimeout;
    var clearT = (timers && timers.clearTimeout) || clearTimeout;
    var handle = null, pendingArgs = null, pendingFn = null;
    function debounced(fn) {
      pendingFn = fn;
      pendingArgs = Array.prototype.slice.call(arguments, 1);
      if (handle !== null) clearT(handle);
      handle = setT(function () {
        handle = null;
        var fnNow = pendingFn, argsNow = pendingArgs;
        pendingFn = null; pendingArgs = null;
        fnNow.apply(null, argsNow);
      }, delay);
    }
    debounced.cancel = function () {
      if (handle !== null) clearT(handle);
      handle = null; pendingFn = null; pendingArgs = null;
    };
    debounced.flush = function () {
      if (handle === null || !pendingFn) return;
      clearT(handle);
      var fnNow = pendingFn, argsNow = pendingArgs;
      handle = null; pendingFn = null; pendingArgs = null;
      fnNow.apply(null, argsNow);
    };
    return debounced;
  }

  /* Search index entries: { id, kind, text, project_key }. */
  function buildIndividualSearchIndex(graph) {
    var entries = [];
    if (!graph) return entries;
    (graph.overview && graph.overview.nodes || []).forEach(function (node) {
      entries.push({ id: node.id, kind: "milestone", project_key: graph.work_id || "",
        text: normalizeText([node.id, node.title].join(" ")) });
    });
    (graph.groups || []).forEach(function (group) {
      (group.tasks || []).forEach(function (task) {
        var agent = task.assignment && (task.assignment.agent_label || task.assignment.agent_id) || "";
        entries.push({ id: task.id, kind: "task", project_key: graph.work_id || "",
          text: normalizeText([task.id, task.title, task.instruction, agent].join(" ")) });
      });
    });
    return entries;
  }

  function buildMasterSearchIndex(view) {
    var entries = [];
    if (!view) return entries;
    (view.projects || []).forEach(function (project) {
      entries.push({ id: "project:" + project.project_key, kind: "project", project_key: project.project_key,
        text: normalizeText([project.project_key].concat(project.labels || []).join(" ")) });
    });
    (view.nodes || []).forEach(function (node) {
      entries.push({ id: node.id, kind: node.kind || "node", project_key: node.project_key || "",
        text: normalizeText([node.id, node.key, node.title, node.name, node.project_key].join(" ")) });
    });
    (view.decisions || []).forEach(function (decision, index) {
      entries.push({ id: "decision:" + index, kind: "decision", project_key: decision.project_key || "",
        text: normalizeText([decision.title, decision.summary].join(" ")) });
    });
    return entries;
  }

  /* Case-insensitive substring over normalized whitespace. */
  function searchMatches(index, query) {
    var q = normalizeText(query);
    var ids = {};
    var count = 0;
    if (!q) return { ids: ids, count: 0, empty: true };
    (index || []).forEach(function (entry) {
      if (entry.text.indexOf(q) !== -1) { ids[entry.id] = true; count += 1; }
    });
    return { ids: ids, count: count, empty: false };
  }

  function matchSummary(count) {
    return count === 1 ? "1 match" : count + " matches";
  }

  /* ------------------------------------------------------------------ *
   * Filters (contract §7.2–§7.4)
   * ------------------------------------------------------------------ */

  function sanitizeStatusTokens(tokens, mode) {
    var allowed = STATUS_TOKENS[mode] || STATUS_TOKENS.individual;
    return uniq((tokens || []).map(function (token) { return String(token || "").trim(); })
      .filter(function (token) { return allowed.indexOf(token) !== -1; }));
  }

  function sanitizeRelTokens(tokens) {
    return uniq((tokens || []).map(function (token) { return String(token || "").trim(); })
      .filter(function (token) { return REL_TOKENS.indexOf(token) !== -1; }));
  }

  function edgeRelToken(edge) {
    if (edge && edge.type && REL_TOKENS.indexOf(edge.type) !== -1) return edge.type;
    var parsed = parseNamespacedId(edge && edge.id);
    if (parsed && parsed.namespace === "dep") return "dep";
    if (parsed && parsed.namespace === "link") return edge && edge.type || "related_to";
    return edge && edge.type || "related_to";
  }

  function edgeEndpoint(endpoint) {
    if (endpoint == null) return { node_id: "", project_key: "" };
    if (typeof endpoint === "string") {
      var parsed = parseNamespacedId(endpoint);
      return { node_id: endpoint, project_key: parsed && parsed.namespace === "project" ? parsed.rest : "" };
    }
    return { node_id: endpoint.node_id || endpoint.id || "", project_key: endpoint.project_key || "" };
  }

  function masterNodeStatusToken(node, projectsByKey) {
    if (!node) return "unknown";
    if ((node.kind || "") === "project") {
      var project = projectsByKey && projectsByKey[node.project_key || (parseNamespacedId(node.id) || {}).rest];
      if (project) {
        if (project.available === false) return "unavailable";
        if (project.catalog_state === "invalid" || project.catalog_state === "unsupported_schema") return "invalid";
        if (project.catalog_state === "missing") return "missing";
        return "available";
      }
      return node.status || "available";
    }
    return node.status || "unknown";
  }

  /* Combined predicate (contract §7.4). Returns per-node and per-edge
   * visibility; graph mode dims non-matches, list mode hides them. */
  function computeMasterVisibility(view, filters) {
    var result = { nodeShown: {}, nodeDimmed: {}, projectShown: {}, edgeShown: {}, matchCount: 0 };
    if (!view) return result;
    var statusSet = sanitizeStatusTokens(filters.status || [], "master");
    var relSet = sanitizeRelTokens(filters.rel || []);
    var index = buildMasterSearchIndex(view);
    var match = searchMatches(index, filters.q);
    result.matchCount = match.count;

    var projectsByKey = {};
    (view.projects || []).forEach(function (project) { projectsByKey[project.project_key] = project; });

    (view.projects || []).forEach(function (project) {
      var projectId = "project:" + project.project_key;
      var projectText = normalizeText([project.project_key].concat(project.labels || []).join(" "));
      var projectMatch = match.empty || projectText.indexOf(normalizeText(filters.q)) !== -1;
      var projectStatus = project.available === false ? "unavailable" :
        (project.catalog_state === "invalid" || project.catalog_state === "unsupported_schema" ? "invalid" :
          (project.catalog_state === "missing" ? "missing" : "available"));
      result.projectShown[projectId] = projectMatch &&
        (statusSet.length === 0 || statusSet.indexOf(projectStatus) !== -1);
    });

    (view.edges || []).forEach(function (edge) {
      result.edgeShown[edge.id] = relSet.length === 0 || relSet.indexOf(edgeRelToken(edge)) !== -1;
    });

    var incidentVisible = {};
    (view.edges || []).forEach(function (edge) {
      if (!result.edgeShown[edge.id]) return;
      incidentVisible[edgeEndpoint(edge.from).node_id] = true;
      incidentVisible[edgeEndpoint(edge.to).node_id] = true;
    });

    (view.nodes || []).forEach(function (node) {
      var directMatch = match.empty ? true : Boolean(match.ids[node.id]);
      var statusOk = statusSet.length === 0 ||
        statusSet.indexOf(masterNodeStatusToken(node, projectsByKey)) !== -1;
      var anchored = Boolean(incidentVisible[node.id]) ||
        (filters.sel && filters.sel === node.id) || directMatch;
      var shown = directMatch && statusOk && anchored;
      result.nodeShown[node.id] = shown;
      result.nodeDimmed[node.id] = !shown;
    });
    return result;
  }

  function computeIndividualVisibility(nodes, filters) {
    var statusSet = sanitizeStatusTokens(filters.status || [], "individual");
    var q = normalizeText(filters.q);
    var shown = {}, count = 0;
    (nodes || []).forEach(function (node) {
      var agent = node.assignment && (node.assignment.agent_label || node.assignment.agent_id) || "";
      var text = normalizeText([node.id, node.title, node.instruction, agent].join(" "));
      var matches = !q || text.indexOf(q) !== -1;
      var statusOk = statusSet.length === 0 || statusSet.indexOf(node.status) !== -1;
      shown[node.id] = matches && statusOk;
      if (q && matches) count += 1;
    });
    return { nodeShown: shown, matchCount: count };
  }

  /* ------------------------------------------------------------------ *
   * Project grouping + large-graph render plan (contract §2.4, §14)
   * ------------------------------------------------------------------ */

  function groupNodesByProject(nodes) {
    var groups = new Map();
    (nodes || []).forEach(function (node) {
      var key = node.project_key || (parseNamespacedId(node.id) || {}).rest || "(unassigned)";
      if (node.kind === "project" && !node.project_key) {
        key = (parseNamespacedId(node.id) || {}).rest || key;
      }
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(node);
    });
    return groups;
  }

  /* Decide what actually gets mounted. Over-budget graphs collapse to one
   * cluster node per project (until expanded) and emit an explicit
   * `graph_truncated` diagnostic — never a silent drop. */
  function planMasterRender(view, options) {
    options = options || {};
    var caps = options.caps || RENDER_BUDGET;
    var expanded = options.expandedProjects || [];
    var visibility = options.visibility || null;
    var allNodes = (view && view.nodes) || [];
    var allEdges = (view && view.edges) || [];
    var diagnostics = [];

    var candidateNodes = visibility
      ? allNodes.filter(function (node) { return visibility.nodeShown[node.id] !== false || options.keepDimmed; })
      : allNodes.slice();

    if (candidateNodes.length <= caps.maxSvgNodes) {
      var edgePlan = capEdgeList(allEdges, candidateNodes, caps.maxSvgEdgePaths, visibility);
      if (edgePlan.truncated > 0) {
        diagnostics.push(truncationDiagnostic("edges", edgePlan.truncated, allEdges.length));
      }
      return { kind: "full", nodes: candidateNodes, edges: edgePlan.edges,
        clusters: null, truncated: { nodes: 0, edges: edgePlan.truncated }, diagnostics: diagnostics };
    }

    // Clustered fallback: one node per project_key, aggregate cross-project edges.
    var groups = groupNodesByProject(candidateNodes);
    var clusters = [];
    var mountedNodes = 0;
    groups.forEach(function (members, key) {
      if (expanded.indexOf(key) !== -1) {
        var room = Math.max(0, caps.maxSvgNodes - mountedNodes);
        members.slice(0, room).forEach(function (node) {
          clusters.push({ cluster: false, id: node.id, title: node.title,
            kind: node.kind, node: node, project_key: key });
          mountedNodes += 1;
        });
      } else {
        clusters.push({ cluster: true, project_key: key, count: members.length,
          id: "cluster:" + key, title: key, kind: "cluster" });
        mountedNodes += 1;
      }
    });
    var projectOf = {};
    candidateNodes.forEach(function (node) {
      projectOf[node.id] = node.project_key || (parseNamespacedId(node.id) || {}).rest || "(unassigned)";
    });
    var aggregate = new Map();
    allEdges.forEach(function (edge) {
      var fromProject = projectOf[edgeEndpoint(edge.from).node_id];
      var toProject = projectOf[edgeEndpoint(edge.to).node_id];
      if (!fromProject || !toProject || fromProject === toProject) return;
      var key = fromProject + "→" + toProject;
      if (!aggregate.has(key)) {
        aggregate.set(key, { id: "cluster-edge:" + key, from: "cluster:" + fromProject,
          to: "cluster:" + toProject, type: "related_to", count: 0 });
      }
      aggregate.get(key).count += 1;
    });
    var clusterEdges = Array.from(aggregate.values()).slice(0, caps.maxSvgEdgePaths);
    var hiddenNodes = Math.max(0, candidateNodes.length - clusters.length);
    diagnostics.push(truncationDiagnostic("nodes", hiddenNodes, candidateNodes.length));
    return { kind: "clustered", nodes: clusters, edges: clusterEdges, clusters: groups,
      truncated: { nodes: hiddenNodes, edges: 0 }, diagnostics: diagnostics };
  }

  function capEdgeList(edges, nodes, maxPaths, visibility) {
    var nodeSet = {};
    (nodes || []).forEach(function (node) { nodeSet[node.id] = true; });
    var mountable = (edges || []).filter(function (edge) {
      if (visibility && visibility.edgeShown[edge.id] === false) return false;
      return nodeSet[edgeEndpoint(edge.from).node_id] && nodeSet[edgeEndpoint(edge.to).node_id];
    });
    // Master mode renders one path per edge (edge-flow omitted, §14), so the
    // path budget equals the edge count budget here.
    if (mountable.length <= maxPaths) return { edges: mountable, truncated: 0 };
    return { edges: mountable.slice(0, maxPaths), truncated: mountable.length - maxPaths };
  }

  function truncationDiagnostic(what, hidden, total) {
    return {
      severity: "info", code: "graph_truncated",
      message: "Graph truncated for rendering budget: " + hidden + " of " + total +
        " " + what + " are not mounted. Use list view, search, or filters to reach them.",
      context: { what: what, hidden: hidden, total: total }
    };
  }

  /* List virtualization (contract §14): virtualize when > 200 rows, keeping
   * the selected row ± overscan mounted. */
  function computeListWindow(totalRows, selectedIndex, caps) {
    caps = caps || RENDER_BUDGET;
    if (totalRows <= caps.virtualizeRowsOver) {
      return { start: 0, end: totalRows, virtualized: false };
    }
    var anchor = Number.isInteger(selectedIndex) && selectedIndex >= 0 ? selectedIndex : 0;
    var start = Math.max(0, anchor - caps.rowOverscan);
    var end = Math.min(totalRows, anchor + caps.rowOverscan + 1);
    if (end - start < caps.rowOverscan * 2) {
      end = Math.min(totalRows, start + caps.rowOverscan * 2);
    }
    return { start: start, end: end, virtualized: true };
  }

  /* ------------------------------------------------------------------ *
   * Cross-link traversal (contract §8)
   * ------------------------------------------------------------------ */

  function resolveCrossLink(edge) {
    if (!edge) return { action: "inspect", reason: "missing_edge" };
    var to = edgeEndpoint(edge.to);
    var resolution = edge.resolution || "unresolved";
    var from = edgeEndpoint(edge.from);
    if (resolution === "self" || (from.node_id && from.node_id === to.node_id)) {
      return { action: "inspect", reason: "self", edge_id: edge.id };
    }
    if (resolution === "resolved" && to.node_id) {
      var parsed = parseNamespacedId(to.node_id);
      var projectKey = to.project_key || (parsed && parsed.namespace === "project" ? parsed.rest : "");
      // A resolved endpoint is only traversable when it names an inventory
      // project. Never turn an incomplete endpoint into a request for the
      // board-bound project or a filesystem path.
      if (!projectKey) {
        return { action: "inspect", reason: "resolved_missing_project", edge_id: edge.id };
      }
      // Only project_key query params ever leave the client — never paths (§8.3).
      return {
        action: "navigate", cyclic: Boolean(edge.cycle_group),
        target: {
          mode: "individual",
          project: projectKey,
          sel: parsed && parsed.namespace !== "project" ? to.node_id : ""
        }
      };
    }
    return { action: "inspect", reason: resolution, edge_id: edge.id };
  }

  function pushBreadcrumb(stack, entry) {
    return (stack || []).concat([{
      from: entry.from || "master",
      edge_id: entry.edge_id || "",
      return_query: entry.return_query || ""
    }]);
  }

  function popBreadcrumb(stack) {
    if (!stack || !stack.length) return { entry: null, stack: [] };
    return { entry: stack[stack.length - 1], stack: stack.slice(0, -1) };
  }

  /* ------------------------------------------------------------------ *
   * Evidence / tests / decisions / diagnostics (contract §9)
   * ------------------------------------------------------------------ */

  function isAbsolutePath(path) {
    return /^(\/|~|[A-Za-z]:[\\/]|\\\\)/.test(String(path || ""));
  }

  /* Absolute paths must never appear (§9.1): keep only the final segment and
   * mark the ref redacted so the UI can say why. */
  function sanitizeEvidenceRef(ref) {
    ref = ref || {};
    var rawPath = String(ref.path || "");
    var redacted = isAbsolutePath(rawPath);
    var segments = rawPath.split(/[\\/]/).filter(Boolean);
    return {
      path: redacted ? (segments[segments.length - 1] || "(redacted)") : rawPath,
      redacted: redacted,
      kind: ref.kind || null,
      sha256: ref.sha256 || null,
      spans: Array.isArray(ref.spans) ? ref.spans : null,
      observed_commit: ref.observed_commit || null,
      dirty: Boolean(ref.dirty)
    };
  }

  function capEvidenceRefs(refs, cap) {
    var limit = Number.isFinite(cap) ? cap : RENDER_BUDGET.evidenceCap;
    var sanitized = (refs || []).map(sanitizeEvidenceRef);
    return {
      shown: sanitized.slice(0, limit),
      hiddenCount: Math.max(0, sanitized.length - limit)
    };
  }

  /* Classifications other than `pass` never imply verified (§9.1). */
  function formatTestEntry(test) {
    test = test || {};
    return {
      command: String(test.command || ""),
      classification: String(test.classification || "unknown"),
      exit_code: Number.isFinite(Number(test.exit_code)) ? Number(test.exit_code) : null,
      duration_ms: Number.isFinite(Number(test.duration_ms)) ? Number(test.duration_ms) : null,
      log_sha256: test.log_sha256 || null,
      log_excerpt: String(test.log_excerpt || "").slice(0, RENDER_BUDGET.logExcerptCap)
    };
  }

  function formatDecisionEntry(decision) {
    decision = decision || {};
    var status = String(decision.status || "proposed");
    if (["adopted", "proposed", "superseded", "rejected"].indexOf(status) === -1) status = "proposed";
    return {
      title: String(decision.title || "(untitled decision)"),
      status: status,
      summary: String(decision.summary || ""),
      evidence: capEvidenceRefs(decision.evidence || []).shown
    };
  }

  /* Empty copy must explain WHY the panel is empty (§9.1). */
  function emptyPanelCopy(panel, context) {
    context = context || {};
    if (context.catalog_state === "missing") {
      return "No " + panel + " yet: this project has not been audited (catalog_state: missing).";
    }
    if (context.catalog_state === "invalid" || context.catalog_state === "unsupported_schema") {
      return "This project's catalog is invalid; " + panel + " cannot be shown without a partial parse.";
    }
    if (context.filtered) {
      return "All " + panel + " entries are hidden by the active filters. Clear filters to restore them.";
    }
    return "No " + panel + " recorded for this selection.";
  }

  function diagnosticCounts(diagnostics) {
    var counts = { error: 0, warning: 0, info: 0 };
    (diagnostics || []).forEach(function (diagnostic) {
      var severity = diagnostic && diagnostic.severity;
      if (counts[severity] != null) counts[severity] += 1;
      else counts.info += 1;
    });
    return counts;
  }

  function diagnosticsForSelection(diagnostics, selection) {
    if (!selection || selection.kind === "none") return (diagnostics || []).slice();
    return (diagnostics || []).filter(function (diagnostic) {
      var context = diagnostic && diagnostic.context || {};
      return context.project_key === selection.project_key ||
        context.node_id === selection.node_id ||
        context.edge_id === selection.edge_id ||
        (selection.kind === "diagnostic");
    });
  }

  /* ------------------------------------------------------------------ *
   * Loading / error / degraded states (contract §3.5, §10)
   * ------------------------------------------------------------------ */

  function errorStateFor(code, message) {
    var copy = ERROR_COPY[code] || ERROR_COPY.network;
    return {
      code: ERROR_COPY[code] ? code : "network",
      title: copy.title,
      message: message || copy.message,
      retryable: copy.retryable
    };
  }

  function setBusy(element, busy) {
    if (!element) return;
    element.setAttribute("aria-busy", busy ? "true" : "false");
    if (element.classList) element.classList.toggle("mg-busy", Boolean(busy));
  }

  /* ------------------------------------------------------------------ *
   * Caches (contract §10.3) — accelerators only, never authoritative
   * ------------------------------------------------------------------ */

  function createKeyedCache() {
    var key = null, value = null;
    return {
      get: function (wantKey) { return key !== null && key === wantKey ? value : null; },
      set: function (newKey, newValue) { key = newKey; value = newValue; },
      invalidate: function () { key = null; value = null; },
      key: function () { return key; }
    };
  }

  function masterCacheKey(view) {
    if (!view) return null;
    return [view.inventory_hash || "", view.view_hash || ""].join("|");
  }

  /* ------------------------------------------------------------------ *
   * GET-only API client with injected fetch (contract §3.1)
   * ------------------------------------------------------------------ */

  function createApiClient(fetchImpl, options) {
    options = options || {};
    var base = options.base || "";
    var now = options.now || function () { return Date.now(); };
    if (typeof fetchImpl !== "function") throw new Error("createApiClient requires an injected fetch function");

    function getJson(path) {
      // Every request this module makes is a loopback GET; mutation routes
      // return 405 on the board and are never called from here.
      return fetchImpl(base + path, { method: "GET" }).then(function (response) {
        return response.json().catch(function () { return null; }).then(function (body) {
          if (!response.ok) {
            var error = new Error((body && body.error) || "Request failed (" + response.status + ")");
            error.code = (body && body.code) ||
              (response.status === 404 ? "not_found" : response.status === 405 ? "read_only" : "bad_request");
            error.status = response.status;
            throw error;
          }
          return body;
        });
      }, function (cause) {
        var error = new Error(cause && cause.message || "Board unreachable");
        error.code = "network";
        throw error;
      });
    }

    return {
      health: function () { return getJson("/api/health"); },
      identity: function () { return getJson("/api/identity"); },
      projects: function () { return getJson("/api/projects"); },
      graph: function (projectKey, bust) {
        var params = new URLSearchParams();
        if (projectKey) params.set("project", projectKey);
        if (bust) params.set("t", String(now()));
        var query = params.toString();
        return getJson("/api/graph" + (query ? "?" + query : ""));
      },
      masterGraph: function () { return getJson("/api/master-graph"); }
    };
  }

  /* ------------------------------------------------------------------ *
   * Keyboard activation + focus restoration (contract §11)
   * ------------------------------------------------------------------ */

  function handleActivationKey(event, activate) {
    if (!event) return false;
    var isSpace = event.key === " " || event.key === "Spacebar";
    if (event.key !== "Enter" && !isSpace) return false;
    // Prevent the browser's follow-up click as well as Space scrolling. This
    // keeps native buttons from activating twice while preserving activation
    // for SVG/list elements in headless tests.
    if (typeof event.preventDefault === "function") event.preventDefault();
    activate(event);
    return true;
  }

  function attachActivation(element, activate) {
    element.addEventListener("click", activate);
    element.addEventListener("keydown", function (event) { handleActivationKey(event, activate); });
  }

  function captureFocusId(doc, root) {
    var active = doc && doc.activeElement;
    if (!active || !root || (root.contains && !root.contains(active))) return null;
    return (active.getAttribute && active.getAttribute("data-node-id")) || active.id || null;
  }

  /* After a rebuild, refocus the same node id when it still exists; otherwise
   * fall back to the stage container (contract §11.3). */
  function restoreFocusById(root, savedId) {
    if (!root) return false;
    if (savedId) {
      var target = root.querySelector('[data-node-id="' + cssEscape(savedId) + '"]');
      if (target && typeof target.focus === "function") { target.focus(); return true; }
    }
    if (typeof root.focus === "function") root.focus();
    return false;
  }

  /* ------------------------------------------------------------------ *
   * Accessible live summaries (contract §11.6)
   * ------------------------------------------------------------------ */

  function createAnnouncer(doc, container) {
    var polite = doc.createElement("div");
    polite.className = "mg-visually-hidden";
    polite.setAttribute("aria-live", "polite");
    var assertive = doc.createElement("div");
    assertive.className = "mg-visually-hidden";
    assertive.setAttribute("aria-live", "assertive");
    assertive.setAttribute("role", "alert");
    if (container) { container.append(polite); container.append(assertive); }
    return {
      politeRegion: polite,
      assertiveRegion: assertive,
      announce: function (message, options) {
        var region = options && options.assertive ? assertive : polite;
        region.textContent = "";
        region.textContent = String(message || "");
      }
    };
  }

  /* ------------------------------------------------------------------ *
   * DOM renderers — every renderer takes (doc, container, …) so tests can
   * inject a JSDOM-style document and detached roots.
   * ------------------------------------------------------------------ */

  function el(doc, tag, className, text) {
    var node = doc.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  function svgEl(doc, name, attrs) {
    var node = doc.createElementNS(SVG_NS, name);
    Object.keys(attrs || {}).forEach(function (key) { node.setAttribute(key, attrs[key]); });
    return node;
  }

  function projectPrimaryLabel(project) {
    if (!project) return "";
    if (project.labels && project.labels.length) return project.labels[0];
    if (project.canonical_workspace) {
      var segments = String(project.canonical_workspace).split(/[\\/]/).filter(Boolean);
      if (segments.length) return segments[segments.length - 1];
    }
    return project.project_key || "";
  }

  function projectOptionName(project) {
    return projectPrimaryLabel(project) + ", " + (project.catalog_state || "unknown") + ", " +
      (project.available === false ? "unavailable" : "available");
  }

  function workspaceLabel(path) {
    var segments = String(path || "").split(/[\\/]/).filter(Boolean);
    return segments[segments.length - 1] || "(unknown workspace)";
  }

  /* Project switcher (contract §4): listbox popup, keyboard operable,
   * unavailable rows disabled with reason text, current row aria-current. */
  function renderProjectSwitcher(doc, container, payload, state, handlers) {
    handlers = handlers || {};
    container.replaceChildren();
    container.classList.add("mg-switcher");

    var opener = el(doc, "button", "mg-switcher-opener");
    opener.type = "button";
    opener.setAttribute("aria-haspopup", "listbox");
    opener.setAttribute("aria-expanded", state.open ? "true" : "false");
    opener.setAttribute("aria-label", "Project switcher");
    var currentProject = (payload && payload.projects || []).find(function (project) {
      return project.project_key === state.currentProjectKey;
    });
    opener.textContent = currentProject ? projectPrimaryLabel(currentProject) : "Select project";
    container.append(opener);

    var list = el(doc, "ul", "mg-switcher-list" + (state.open ? "" : " mg-hidden"));
    list.setAttribute("role", "listbox");
    list.setAttribute("aria-label", "Project switcher");
    container.append(list);

    var options = [];
    (payload && payload.projects || []).forEach(function (project) {
      var row = el(doc, "li", "mg-switcher-option");
      row.setAttribute("role", "option");
      row.setAttribute("data-project-key", project.project_key);
      row.setAttribute("aria-label", projectOptionName(project));
      row.tabIndex = -1;
      if (project.project_key === state.currentProjectKey) row.setAttribute("aria-current", "true");
      var selectable = project.available !== false;
      if (!selectable) {
        row.classList.add("mg-disabled");
        row.setAttribute("aria-disabled", "true");
      }
      row.append(el(doc, "span", "mg-switcher-label", projectPrimaryLabel(project)));
      row.append(el(doc, "span", "mg-switcher-state mg-status-" +
        (project.available === false ? "unavailable" : (project.catalog_state || "unknown")),
        project.available === false
          ? "unavailable" + (project.unavailable_reason ? " · " + project.unavailable_reason : "")
          : (project.catalog_state || "unknown")));
      attachActivation(row, function () {
        if (handlers.onSelect) handlers.onSelect(project, { available: selectable });
      });
      list.append(row);
      options.push(row);
    });
    (payload && payload.unavailable || []).forEach(function (entry) {
      var row = el(doc, "li", "mg-switcher-option mg-disabled");
      row.setAttribute("role", "option");
      row.setAttribute("aria-disabled", "true");
      var unavailableLabel = workspaceLabel(entry.canonical_workspace) || entry.project_key || "(unknown workspace)";
      row.setAttribute("aria-label", unavailableLabel + ", unavailable, unavailable");
      row.append(el(doc, "span", "mg-switcher-label", unavailableLabel));
      row.append(el(doc, "span", "mg-switcher-state mg-status-unavailable",
        "unavailable · " + (entry.reason || "unknown reason")));
      attachActivation(row, function () {
        if (handlers.onSelectUnavailable) handlers.onSelectUnavailable(entry);
      });
      list.append(row);
      options.push(row);
    });

    attachActivation(opener, function () { if (handlers.onToggle) handlers.onToggle(); });
    container.addEventListener("keydown", function (event) {
      if (event.key === "Escape") {
        if (handlers.onClose) handlers.onClose();
        opener.focus();
        return;
      }
      if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
      event.preventDefault();
      var focusable = options.filter(function (row) { return !row.classList.contains("mg-hidden"); });
      if (!focusable.length) return;
      var index = focusable.indexOf(doc.activeElement);
      var next = event.key === "ArrowDown"
        ? focusable[Math.min(focusable.length - 1, index + 1)]
        : focusable[Math.max(0, index <= 0 ? 0 : index - 1)];
      next.focus();
    });
    if (state.open && options.length) options[0].tabIndex = 0;
    return { opener: opener, list: list, options: options };
  }

  /* Mode toggle (contract §5.1): radiogroup labeled "View mode". */
  function renderModeToggle(doc, container, mode, onChange) {
    container.replaceChildren();
    container.classList.add("mg-mode-toggle");
    container.setAttribute("role", "radiogroup");
    container.setAttribute("aria-label", "View mode");
    MODES.forEach(function (candidate) {
      var button = el(doc, "button", "mg-mode-option" + (candidate === mode ? " mg-active" : ""));
      button.type = "button";
      button.setAttribute("role", "radio");
      button.setAttribute("aria-checked", candidate === mode ? "true" : "false");
      button.setAttribute("data-mode", candidate);
      button.textContent = candidate === "individual" ? "Individual" : "Master";
      attachActivation(button, function () { if (candidate !== mode && onChange) onChange(candidate); });
      container.append(button);
    });
  }

  /* Search control (contract §7.1). */
  function renderSearchControl(doc, container, value, onInput) {
    container.replaceChildren();
    container.classList.add("mg-search");
    var input = el(doc, "input", "mg-search-input");
    input.type = "search";
    input.value = value || "";
    input.maxLength = RENDER_BUDGET.maxQueryLength;
    input.setAttribute("aria-label", "Search projects, components, and features");
    input.placeholder = "Search";
    input.addEventListener("input", function () { if (onInput) onInput(input.value); });
    container.append(input);
    return input;
  }

  /* Status + relationship filter chip groups (contract §7.2–§7.3). */
  function renderFilterGroup(doc, container, label, tokens, active, onToggle) {
    container.replaceChildren();
    container.classList.add("mg-filter-group");
    container.setAttribute("role", "group");
    container.setAttribute("aria-label", label);
    tokens.forEach(function (token) {
      var pressed = active.indexOf(token) !== -1;
      var chip = el(doc, "button", "mg-filter-chip" + (pressed ? " mg-active" : ""));
      chip.type = "button";
      chip.setAttribute("aria-pressed", pressed ? "true" : "false");
      chip.setAttribute("data-token", token);
      chip.textContent = STATUS_LABELS[token] || token.replace(/_/g, " ");
      attachActivation(chip, function () { if (onToggle) onToggle(token, !pressed); });
      container.append(chip);
    });
  }

  function renderStatusFilter(doc, container, mode, active, onToggle) {
    renderFilterGroup(doc, container, "Filter by status", STATUS_TOKENS[mode] || [], active, onToggle);
  }

  function renderRelationshipFilter(doc, container, active, onToggle) {
    renderFilterGroup(doc, container, "Filter by relationship type", REL_TOKENS, active, onToggle);
  }

  /* Hero metric relabeling for master mode (contract §3.4). */
  function masterHeroMetrics(view) {
    var summary = view && view.summary || {};
    var counts = summary.diagnostic_counts || {};
    return [
      { id: "percent", label: "PROJECTS", value: String(summary.projects_total != null ? summary.projects_total : "—") },
      { id: "completed", label: "AUDITED", value: String(summary.audited_available != null ? summary.audited_available : "—") },
      { id: "active", label: "NODES", value: String(summary.node_count != null ? summary.node_count : "—") },
      { id: "remaining", label: "ISSUES", value: String((counts.error || 0) + (counts.warning || 0)) }
    ];
  }

  function masterNodeAccessibleName(node, projectsByKey) {
    var parsed = parseNamespacedId(node && node.id);
    var projectKey = (node && node.project_key) ||
      (parsed && parsed.namespace === "project" ? parsed.rest : "");
    var project = projectsByKey && projectsByKey[projectKey];
    var label = project ? projectPrimaryLabel(project) : (projectKey || "estate");
    var status = STATUS_LABELS[masterNodeStatusToken(node, projectsByKey)] || "Unknown";
    return (node.kind || "node") + ", " + (node.title || node.name || node.id) + ", " + status + ", project " + label;
  }

  function edgeAccessibleName(edge) {
    return edgeRelToken(edge).replace(/_/g, " ") + " from " + edgeEndpoint(edge.from).node_id +
      " to " + edgeEndpoint(edge.to).node_id + ", " + (edge.resolution || "resolved");
  }

  /* Deterministic cluster-column layout grouped by project_key (§2.4). */
  function layoutMasterGraph(plan) {
    var positions = {};
    var nodeWidth = 216, nodeHeight = 66, columnGap = 260, rowGap = 88, topPad = 70;
    var groups = new Map();
    plan.nodes.forEach(function (item) {
      var node = item.cluster ? item : item;
      var key = item.cluster ? item.project_key : (item.project_key || (parseNamespacedId(item.id) || {}).rest || "(unassigned)");
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(node);
    });
    var column = 0;
    var maxRows = 1;
    Array.from(groups.keys()).sort().forEach(function (key) {
      var members = groups.get(key);
      members.sort(function (a, b) { return String(a.id).localeCompare(String(b.id)); });
      members.forEach(function (node, row) {
        positions[node.id] = [140 + column * columnGap, topPad + 40 + row * rowGap];
        maxRows = Math.max(maxRows, row + 1);
      });
      column += 1;
    });
    return {
      positions: positions,
      nodeWidth: nodeWidth,
      nodeHeight: nodeHeight,
      contentWidth: Math.max(1000, 160 + column * columnGap),
      contentHeight: Math.max(560, topPad + 60 + maxRows * rowGap)
    };
  }

  /* Master SVG stage. Single path per edge (no edge-flow, §14). */
  function renderMasterSvg(doc, svg, view, plan, state, handlers) {
    handlers = handlers || {};
    svg.replaceChildren();
    var projectsByKey = {};
    (view && view.projects || []).forEach(function (project) { projectsByKey[project.project_key] = project; });
    var shortHash = String(view && view.inventory_hash || "").replace(/^sha256:/, "").slice(0, 8);
    svg.setAttribute("role", "img");
    svg.setAttribute("aria-label", "Master graph" + (shortHash ? " · inventory " + shortHash : ""));
    var layout = layoutMasterGraph(plan);
    svg.setAttribute("viewBox", "0 0 " + layout.contentWidth + " " + layout.contentHeight);

    var edgeLayer = svgEl(doc, "g", {});
    plan.edges.forEach(function (edge) {
      var from = layout.positions[edgeEndpoint(edge.from).node_id] || layout.positions[edge.from];
      var to = layout.positions[edgeEndpoint(edge.to).node_id] || layout.positions[edge.to];
      if (!from || !to) return;
      var x1 = from[0] + layout.nodeWidth / 2, x2 = to[0] - layout.nodeWidth / 2;
      var bend = Math.max(36, Math.abs(x2 - x1) * 0.45);
      var dimmed = state.visibility && state.visibility.edgeShown[edge.id] === false;
      var selected = state.sel === edge.id;
      var path = svgEl(doc, "path", {
        d: "M " + x1 + " " + from[1] + " C " + (x1 + bend) + " " + from[1] + ", " + (x2 - bend) + " " + to[1] + ", " + x2 + " " + to[1],
        class: "mg-edge mg-edge-" + ((parseNamespacedId(edge.id) || {}).namespace || "dep") +
          (dimmed ? " mg-dim" : "") + (selected ? " mg-selected" : ""),
        "data-node-id": edge.id, tabindex: "0", role: "button",
        "aria-label": edgeAccessibleName(edge)
      });
      attachActivation(path, function () { if (handlers.onSelectEdge) handlers.onSelectEdge(edge); });
      edgeLayer.append(path);
    });
    svg.append(edgeLayer);

    plan.nodes.forEach(function (node) {
      var position = layout.positions[node.id];
      if (!position) return;
      var isCluster = Boolean(node.cluster);
      var statusToken = isCluster ? "available" : masterNodeStatusToken(node, projectsByKey);
      var dimmed = !isCluster && state.visibility && state.visibility.nodeDimmed[node.id];
      var selected = state.sel === node.id;
      var group = svgEl(doc, "g", {
        class: "mg-node mg-status-" + statusToken + (dimmed ? " mg-dim" : "") + (selected ? " mg-selected" : ""),
        transform: "translate(" + position[0] + "," + position[1] + ")",
        tabindex: "0", role: "button", "data-node-id": node.id,
        "aria-label": isCluster
          ? "cluster, " + node.project_key + ", " + node.count + " nodes, activate to expand"
          : masterNodeAccessibleName(node, projectsByKey)
      });
      group.append(svgEl(doc, "rect", {
        class: "mg-node-body", x: -layout.nodeWidth / 2, y: -layout.nodeHeight / 2,
        width: layout.nodeWidth, height: layout.nodeHeight, rx: 5
      }));
      var kindText = svgEl(doc, "text", { class: "mg-node-kind", x: -layout.nodeWidth / 2 + 14, y: -layout.nodeHeight / 2 + 18 });
      kindText.textContent = isCluster ? node.count + " NODES" : String(node.kind || "node").toUpperCase();
      group.append(kindText);
      var titleText = svgEl(doc, "text", { class: "mg-node-title", x: -layout.nodeWidth / 2 + 14, y: 4 });
      titleText.textContent = clampText(node.title || node.name || node.id, 26);
      group.append(titleText);
      var statusText = svgEl(doc, "text", { class: "mg-node-status", x: -layout.nodeWidth / 2 + 14, y: layout.nodeHeight / 2 - 10 });
      statusText.textContent = isCluster
        ? "PROJECT CLUSTER"
        : (STATUS_LABELS[statusToken] || statusToken).toUpperCase();
      group.append(statusText);
      attachActivation(group, function () {
        if (isCluster) { if (handlers.onExpandCluster) handlers.onExpandCluster(node.project_key); }
        else if (handlers.onSelectNode) handlers.onSelectNode(node);
      });
      svg.append(group);
    });
  }

  /* List/detail fallback rows (contract §13): every searchable node appears
   * as a row with visible title + status text; virtualized past 200 rows. */
  function buildMasterListRows(view, visibility) {
    var projectsByKey = {};
    (view && view.projects || []).forEach(function (project) { projectsByKey[project.project_key] = project; });
    var rows = [];
    (view && view.projects || []).forEach(function (project) {
      rows.push({
        id: "project:" + project.project_key, kind: "project",
        title: projectPrimaryLabel(project),
        status: project.available === false ? "unavailable" : (project.catalog_state || "unknown"),
        project_key: project.project_key
      });
    });
    (view && view.nodes || []).forEach(function (node) {
      if (node.kind === "project" && projectsByKey[node.project_key]) return; // already listed
      rows.push({
        id: node.id, kind: node.kind || "node",
        title: node.title || node.name || node.id,
        status: masterNodeStatusToken(node, projectsByKey),
        project_key: node.project_key || ""
      });
    });
    if (visibility) {
      rows = rows.filter(function (row) {
        if (row.kind === "project") {
          // Project rows are synthetic when the payload has no project node.
          // Keep them in sync with the same search/status predicate as nodes.
          if (visibility.projectShown && visibility.projectShown[row.id] === false) return false;
          return true;
        }
        return visibility.nodeShown[row.id] !== false;
      });
    }
    return rows;
  }

  function renderMasterList(doc, container, rows, state, handlers) {
    handlers = handlers || {};
    container.replaceChildren();
    container.classList.add("mg-list");
    container.setAttribute("role", "listbox");
    container.setAttribute("aria-label", "Master graph list");
    var selectedIndex = rows.findIndex(function (row) { return row.id === state.sel; });
    var windowPlan = computeListWindow(rows.length, selectedIndex, state.caps);
    if (windowPlan.virtualized && windowPlan.start > 0) {
      container.append(el(doc, "div", "mg-list-gap",
        windowPlan.start + " earlier rows — search or filter to narrow"));
    }
    rows.slice(windowPlan.start, windowPlan.end).forEach(function (row) {
      var item = el(doc, "div", "mg-list-row mg-status-" + row.status + (row.id === state.sel ? " mg-selected" : ""));
      item.setAttribute("role", "option");
      item.setAttribute("aria-selected", row.id === state.sel ? "true" : "false");
      item.setAttribute("data-node-id", row.id);
      item.tabIndex = 0;
      item.append(el(doc, "span", "mg-list-kind", row.kind));
      item.append(el(doc, "span", "mg-list-title", row.title));
      item.append(el(doc, "span", "mg-list-status", STATUS_LABELS[row.status] || row.status));
      attachActivation(item, function () { if (handlers.onSelectRow) handlers.onSelectRow(row); });
      container.append(item);
    });
    if (windowPlan.virtualized && windowPlan.end < rows.length) {
      container.append(el(doc, "div", "mg-list-gap",
        (rows.length - windowPlan.end) + " later rows — search or filter to narrow"));
    }
    return windowPlan;
  }

  /* Inspector detail panels (contract §9): info / evidence / tests /
   * decisions / diagnostics with `panel` URL binding. */
  function renderDetailPanels(doc, container, model, activePanel, handlers) {
    handlers = handlers || {};
    model = model || {};
    container.replaceChildren();
    container.classList.add("mg-detail");

    var tabs = el(doc, "div", "mg-panel-tabs");
    tabs.setAttribute("role", "tablist");
    tabs.setAttribute("aria-label", "Selection detail panels");
    PANELS.forEach(function (panel) {
      var tab = el(doc, "button", "mg-panel-tab" + (panel === activePanel ? " mg-active" : ""));
      tab.type = "button";
      tab.setAttribute("role", "tab");
      tab.setAttribute("aria-selected", panel === activePanel ? "true" : "false");
      tab.setAttribute("data-panel", panel);
      tab.textContent = panel.charAt(0).toUpperCase() + panel.slice(1);
      attachActivation(tab, function () { if (handlers.onPanelChange) handlers.onPanelChange(panel); });
      tabs.append(tab);
    });
    container.append(tabs);

    var body = el(doc, "div", "mg-panel-body");
    body.setAttribute("role", "tabpanel");
    container.append(body);
    var context = { catalog_state: model.catalog_state, filtered: model.filtered };

    if (activePanel === "info") {
      renderInfoPanel(doc, body, model);
    } else if (activePanel === "evidence") {
      var evidence = capEvidenceRefs(model.evidence || []);
      if (!evidence.shown.length) body.append(el(doc, "p", "mg-empty-copy", emptyPanelCopy("evidence", context)));
      evidence.shown.forEach(function (ref) {
        var row = el(doc, "div", "mg-evidence-row");
        row.append(el(doc, "code", "mg-evidence-path", ref.path + (ref.redacted ? " (path redacted)" : "")));
        var meta = [ref.kind, ref.sha256 ? "sha256:" + String(ref.sha256).replace(/^sha256:/, "").slice(0, 12) + "…" : null,
          ref.observed_commit ? "commit " + String(ref.observed_commit).slice(0, 8) : null,
          ref.dirty ? "dirty worktree" : null].filter(Boolean).join(" · ");
        if (meta) row.append(el(doc, "small", "mg-evidence-meta", meta));
        body.append(row);
      });
      if (evidence.hiddenCount > 0) {
        var more = el(doc, "button", "mg-show-more", "Show " + evidence.hiddenCount + " more evidence refs");
        more.type = "button";
        attachActivation(more, function () { if (handlers.onShowMoreEvidence) handlers.onShowMoreEvidence(); });
        body.append(more);
      }
    } else if (activePanel === "tests") {
      var tests = (model.tests || []).map(formatTestEntry);
      if (!tests.length) body.append(el(doc, "p", "mg-empty-copy", emptyPanelCopy("tests", context)));
      tests.forEach(function (test) {
        var row = el(doc, "div", "mg-test-row");
        row.append(el(doc, "code", "mg-test-command", test.command || "(no command recorded)"));
        row.append(el(doc, "span", "mg-test-class mg-test-" + test.classification,
          test.classification + (test.exit_code != null ? " · exit " + test.exit_code : "") +
          (test.duration_ms != null ? " · " + test.duration_ms + "ms" : "")));
        if (test.classification !== "pass") {
          row.append(el(doc, "small", "mg-test-note", "This classification does not imply verified."));
        }
        if (test.log_excerpt) {
          var pre = el(doc, "pre", "mg-test-log", test.log_excerpt);
          row.append(pre);
        }
        body.append(row);
      });
    } else if (activePanel === "decisions") {
      var decisions = (model.decisions || []).map(formatDecisionEntry);
      if (!decisions.length) body.append(el(doc, "p", "mg-empty-copy", emptyPanelCopy("decisions", context)));
      decisions.forEach(function (decision) {
        var row = el(doc, "div", "mg-decision-row");
        row.append(el(doc, "strong", "mg-decision-title", decision.title));
        row.append(el(doc, "span", "mg-decision-status mg-decision-" + decision.status, decision.status));
        if (decision.summary) row.append(el(doc, "p", "mg-decision-summary", decision.summary));
        if (decision.evidence.length) {
          var refs = decision.evidence.map(function (ref) {
            return ref.path + (ref.redacted ? " (path redacted)" : "");
          }).join(" · ");
          row.append(el(doc, "small", "mg-evidence-meta", "evidence: " + refs));
        }
        body.append(row);
      });
    } else if (activePanel === "diagnostics") {
      var diagnostics = model.diagnostics || [];
      if (!diagnostics.length) body.append(el(doc, "p", "mg-empty-copy", emptyPanelCopy("diagnostics", context)));
      diagnostics.forEach(function (diagnostic) {
        var row = el(doc, "div", "mg-diagnostic-row mg-diag-" + (diagnostic.severity || "info"));
        row.append(el(doc, "span", "mg-diag-severity", (diagnostic.severity || "info").toUpperCase()));
        row.append(el(doc, "span", "mg-diag-code", diagnostic.code || ""));
        row.append(el(doc, "p", "mg-diag-message", diagnostic.message || ""));
        body.append(row);
      });
    }
    return body;
  }

  function renderInfoPanel(doc, body, model) {
    if (!model.selection || model.selection.kind === "none") {
      body.append(el(doc, "p", "mg-empty-copy", "Select a project, node, edge, or diagnostic to inspect it."));
      return;
    }
    var selection = model.selection;
    body.append(el(doc, "p", "mg-info-kind", String(selection.kind).replace(/_/g, " ").toUpperCase()));
    if (model.title) body.append(el(doc, "h3", "mg-info-title", model.title));
    if (model.status) {
      body.append(el(doc, "span", "mg-status-pill mg-status-" + model.status,
        STATUS_LABELS[model.status] || model.status));
    }
    var fields = model.fields || [];
    if (fields.length) {
      var list = el(doc, "dl", "mg-info-fields");
      fields.forEach(function (field) {
        var wrap = el(doc, "div", "");
        wrap.append(el(doc, "dt", "", field.label));
        wrap.append(el(doc, "dd", "", field.value == null || field.value === "" ? "—" : String(field.value)));
        list.append(wrap);
      });
      body.append(list);
    }
  }

  /* Build the inspector model for a selection against the master view. */
  function buildSelectionModel(selection, view) {
    var model = { selection: selection, title: "", status: "", fields: [],
      evidence: [], tests: [], decisions: [], diagnostics: [], catalog_state: null };
    if (!view || !selection || selection.kind === "none") return model;
    var projectsByKey = {};
    (view.projects || []).forEach(function (project) { projectsByKey[project.project_key] = project; });

    if (selection.kind === "project") {
      var project = projectsByKey[selection.project_key];
      model.title = project ? projectPrimaryLabel(project) : selection.project_key;
      model.catalog_state = project ? project.catalog_state : null;
      model.status = project ? (project.available === false ? "unavailable" : (project.catalog_state === "valid" ? "available" : project.catalog_state || "unknown")) : "unknown";
      if (project) {
        model.fields = [
          { label: "PROJECT KEY", value: project.project_key },
          { label: "CATALOG STATE", value: project.catalog_state },
          { label: "AVAILABLE", value: project.available === false ? "no" : "yes" },
          { label: "STATUS COUNTS", value: project.status_counts ? JSON.stringify(project.status_counts) : "" }
        ];
      }
      model.decisions = (view.decisions || []).filter(function (decision) {
        return decision.project_key === selection.project_key;
      });
      model.tests = (view.tests || []).filter(function (test) {
        return test.project_key === selection.project_key;
      });
    } else if (selection.kind === "master_node") {
      var node = (view.nodes || []).find(function (candidate) { return candidate.id === selection.node_id; });
      if (node) {
        model.title = node.title || node.name || node.id;
        model.status = masterNodeStatusToken(node, projectsByKey);
        model.catalog_state = (projectsByKey[node.project_key] || {}).catalog_state || null;
        model.fields = [
          { label: "NODE ID", value: node.id },
          { label: "KIND", value: node.kind },
          { label: "PROJECT", value: node.project_key }
        ];
        model.evidence = node.evidence || [];
        model.tests = (node.tests || (view.tests || []).filter(function (test) {
          return test.project_key === node.project_key ||
            (Array.isArray(node.test_keys) && node.test_keys.indexOf(test.key) !== -1);
        }));
        model.decisions = node.decisions || (view.decisions || []).filter(function (decision) {
          return decision.project_key === node.project_key;
        });
      }
    } else if (selection.kind === "master_edge") {
      var edge = (view.edges || []).find(function (candidate) { return candidate.id === selection.edge_id; });
      if (edge) {
        model.title = edgeRelToken(edge).replace(/_/g, " ");
        model.status = edge.resolution === "resolved" ? "verified" : "unknown";
        model.fields = [
          { label: "EDGE ID", value: edge.id },
          { label: "TYPE", value: edgeRelToken(edge) },
          { label: "FROM", value: edgeEndpoint(edge.from).node_id },
          { label: "TO", value: edgeEndpoint(edge.to).node_id },
          { label: "RESOLUTION", value: edge.resolution || "resolved" },
          { label: "CYCLE GROUP", value: edge.cycle_group || "" },
          { label: "CONFIDENCE", value: edge.confidence != null ? String(edge.confidence) : "" },
          { label: "RATIONALE", value: edge.rationale || "" }
        ];
      }
    } else if (selection.kind === "diagnostic") {
      var diagnostic = (view.diagnostics || [])[selection.diagnostic_index];
      if (diagnostic) {
        model.title = diagnostic.code || "diagnostic";
        model.fields = [
          { label: "SEVERITY", value: diagnostic.severity },
          { label: "MESSAGE", value: diagnostic.message },
          { label: "CONTEXT", value: diagnostic.context ? JSON.stringify(diagnostic.context) : "" }
        ];
      }
    }
    model.diagnostics = diagnosticsForSelection(view.diagnostics || [], selection);
    return model;
  }

  /* Diagnostics rail (contract §2.2): compact counts + expandable list. */
  function renderDiagnosticsRail(doc, container, diagnostics, unavailable, handlers) {
    handlers = handlers || {};
    container.replaceChildren();
    container.classList.add("mg-diagnostics-rail");
    var counts = diagnosticCounts(diagnostics);
    var details = el(doc, "details", "mg-diagnostics");
    var summary = el(doc, "summary", "mg-diagnostics-summary",
      "Diagnostics · " + counts.error + " errors · " + counts.warning + " warnings" +
      ((unavailable || []).length ? " · " + unavailable.length + " unavailable" : ""));
    details.append(summary);
    var list = el(doc, "ul", "mg-diagnostics-list");
    (diagnostics || []).forEach(function (diagnostic, index) {
      var item = el(doc, "li", "mg-diagnostic-row mg-diag-" + (diagnostic.severity || "info"));
      item.tabIndex = 0;
      item.setAttribute("role", "button");
      item.setAttribute("data-node-id", "diag:" + index);
      item.textContent = (diagnostic.severity || "info").toUpperCase() + " · " +
        (diagnostic.code || "") + " · " + (diagnostic.message || "");
      attachActivation(item, function () {
        if (handlers.onSelectDiagnostic) handlers.onSelectDiagnostic(diagnostic, index);
      });
      list.append(item);
    });
    (unavailable || []).forEach(function (entry) {
      var item = el(doc, "li", "mg-diagnostic-row mg-diag-warning");
      item.textContent = "UNAVAILABLE · " + workspaceLabel(entry.canonical_workspace) + " · " + (entry.reason || "");
      list.append(item);
    });
    details.append(list);
    container.append(details);
  }

  /* Degraded / error panel (contract §10.1) — never a blank document. */
  function renderDegradedPanel(doc, container, errorInfo, handlers) {
    handlers = handlers || {};
    container.replaceChildren();
    var panel = el(doc, "div", "mg-degraded");
    panel.setAttribute("role", "status");
    panel.append(el(doc, "h3", "mg-degraded-title", errorInfo.title));
    panel.append(el(doc, "p", "mg-degraded-message", errorInfo.message));
    panel.append(el(doc, "small", "mg-degraded-code", "code: " + errorInfo.code));
    if (errorInfo.retryable && handlers.onRetry) {
      var retry = el(doc, "button", "mg-retry", "Retry");
      retry.type = "button";
      attachActivation(retry, handlers.onRetry);
      panel.append(retry);
    }
    container.append(panel);
    return panel;
  }

  function renderSkeleton(doc, container, label) {
    container.replaceChildren();
    setBusy(container, true);
    var skeleton = el(doc, "div", "mg-skeleton");
    skeleton.append(el(doc, "div", "mg-skeleton-bar", ""));
    skeleton.append(el(doc, "div", "mg-skeleton-bar", ""));
    skeleton.append(el(doc, "div", "mg-skeleton-bar mg-short", ""));
    skeleton.append(el(doc, "p", "mg-skeleton-label", label || "Loading…"));
    container.append(skeleton);
    return skeleton;
  }

  /* Breadcrumb back control (contract §8.2). */
  function renderBreadcrumb(doc, container, stack, onBack) {
    container.replaceChildren();
    if (!stack || !stack.length) { container.classList.add("mg-hidden"); return null; }
    container.classList.remove("mg-hidden");
    var button = el(doc, "button", "mg-breadcrumb", "‹ Back to master");
    button.type = "button";
    attachActivation(button, function () { if (onBack) onBack(); });
    container.append(button);
    return button;
  }

  /* ------------------------------------------------------------------ *
   * Browser controller — orchestrates state, fetch, URL, and renderers
   * against injected roots. All side effects flow through injections.
   * ------------------------------------------------------------------ */

  function createMasterGraphBrowser(options) {
    options = options || {};
    var doc = options.doc || (typeof document !== "undefined" ? document : null);
    if (!doc) throw new Error("createMasterGraphBrowser requires an injected document");
    var root = options.root;
    if (!root) throw new Error("createMasterGraphBrowser requires an injected root element");
    var api = options.api || createApiClient(options.fetchImpl, { now: options.now });
    var history = options.history || null;
    var getSearch = options.getSearch || function () { return ""; };
    var getViewportWidth = options.getViewportWidth || function () { return 1440; };
    var timers = options.timers || null;
    var caps = options.caps || RENDER_BUDGET;

    var regions = {};
    ["chrome", "metrics", "stage", "inspector", "diagnostics", "footer", "live"].forEach(function (name) {
      var region = doc.createElement("section");
      region.className = "mg-region mg-region-" + name;
      if (name === "stage") { region.tabIndex = -1; region.setAttribute("data-region", "stage"); }
      root.append(region);
      regions[name] = region;
    });
    var announcer = options.announcer || createAnnouncer(doc, regions.live);
    var debouncedSearch = createDebouncer(caps.searchDebounceMs, timers);

    var state = {
      query: parseQueryState(getSearch()),
      projects: null,
      graph: null,
      master: null,
      breadcrumbs: [],
      switcherOpen: false,
      expandedProjects: [],
      error: null,
      loading: false,
      focusMemo: null
    };
    var projectsCache = createKeyedCache();
    var masterCache = createKeyedCache();

    function syncUrl(push) {
      if (history) applyQueryState(history, state.query, { push: push });
    }

    function announce(message, assertive) {
      announcer.announce(message, { assertive: Boolean(assertive) });
    }

    function activeView() {
      return effectiveView(state.query, getViewportWidth());
    }

    function visibility() {
      if (state.query.mode !== "master" || !state.master) return null;
      return computeMasterVisibility(state.master, {
        q: state.query.q, status: state.query.status, rel: state.query.rel, sel: state.query.sel
      });
    }

    function render() {
      state.focusMemo = captureFocusId(doc, regions.stage);
      renderChrome();
      renderStage();
      renderInspector();
      renderDiagnosticsRegion();
      renderFooter();
      restoreFocusById(regions.stage, state.focusMemo);
    }

    function renderChrome() {
      var chrome = regions.chrome;
      chrome.replaceChildren();
      var switcherHost = el(doc, "div", "");
      var modeHost = el(doc, "div", "");
      var breadcrumbHost = el(doc, "div", "");
      var searchHost = el(doc, "div", "");
      var statusHost = el(doc, "div", "");
      var relHost = el(doc, "div", "");
      // Tab order per §11.1: switcher, mode, breadcrumb, search, status, rel.
      chrome.append(switcherHost, modeHost, breadcrumbHost, searchHost, statusHost, relHost);

      renderProjectSwitcher(doc, switcherHost, state.projects,
        { open: state.switcherOpen, currentProjectKey: state.query.project ||
          (state.projects && state.projects.bound_project_key) || "" },
        {
          onToggle: function () { state.switcherOpen = !state.switcherOpen; renderChrome(); },
          onClose: function () { state.switcherOpen = false; renderChrome(); },
          onSelect: function (project, meta) {
            state.switcherOpen = false;
            if (!meta.available) { showDegraded(errorStateFor("unavailable_project")); return; }
            selectProject(project.project_key);
          },
          onSelectUnavailable: function () {
            state.switcherOpen = false;
            showDegraded(errorStateFor("unavailable_project"));
            render();
          }
        });

      renderModeToggle(doc, modeHost, state.query.mode, setMode);
      renderBreadcrumb(doc, breadcrumbHost, state.breadcrumbs, goBackToMaster);
      renderSearchControl(doc, searchHost, state.query.q, function (value) {
        debouncedSearch(function () { applySearch(value); });
      });
      renderStatusFilter(doc, statusHost, state.query.mode, state.query.status, function (token, on) {
        toggleStatusToken(token, on);
      });
      if (state.query.mode === "master") {
        renderRelationshipFilter(doc, relHost, state.query.rel, function (token, on) {
          toggleRelToken(token, on);
        });
      }
    }

    function renderStage() {
      var stage = regions.stage;
      if (state.loading) { renderSkeleton(doc, stage, "Loading graph…"); return; }
      setBusy(stage, false);
      if (state.error) { renderDegradedPanel(doc, stage, state.error, { onRetry: refresh }); return; }
      stage.replaceChildren();
      if (state.query.mode === "master") {
        if (!state.master) { renderSkeleton(doc, stage, "Loading master view…"); return; }
        var vis = visibility();
        if (activeView() === "list") {
          var rows = buildMasterListRows(state.master, vis);
          renderMasterList(doc, stage, rows, { sel: state.query.sel, caps: caps }, {
            onSelectRow: function (row) { select(row.id, true); }
          });
        } else {
          var plan = planMasterRender(state.master,
            { caps: caps, visibility: vis, keepDimmed: true, expandedProjects: state.expandedProjects });
          var svg = svgEl(doc, "svg", { class: "mg-master-svg" });
          stage.append(svg);
          renderMasterSvg(doc, svg, state.master, plan, { sel: state.query.sel, visibility: vis }, {
            onSelectNode: function (node) { select(node.id, true); },
            onSelectEdge: function (edge) { activateEdge(edge); },
            onExpandCluster: function (projectKey) {
              state.expandedProjects = uniq(state.expandedProjects.concat([projectKey]));
              render();
            }
          });
          plan.diagnostics.forEach(function (diagnostic) { announce(diagnostic.message); });
        }
      } else {
        // Individual stage rendering stays owned by app.js; this module only
        // surfaces a list fallback body when asked for view=list.
        if (!state.graph) { renderSkeleton(doc, stage, "Loading project graph…"); return; }
        var tasks = [];
        (state.graph.groups || []).forEach(function (group) { tasks = tasks.concat(group.tasks || []); });
        var indVis = computeIndividualVisibility(tasks, { q: state.query.q, status: state.query.status });
        var listRows = tasks.filter(function (task) { return indVis.nodeShown[task.id] !== false; })
          .map(function (task) {
            return { id: task.id, kind: task.kind || "task", title: task.title, status: task.status,
              project_key: state.query.project };
          });
        renderMasterList(doc, stage, listRows, { sel: state.query.sel, caps: caps }, {
          onSelectRow: function (row) { select(row.id, true); }
        });
      }
    }

    function renderInspector() {
      var selection = classifySelection(state.query.sel, state.query.mode);
      var model;
      if (state.query.mode === "master") {
        model = buildSelectionModel(selection, state.master);
      } else {
        model = { selection: selection, title: "", status: "", fields: [], evidence: [], tests: [], decisions: [], diagnostics: [] };
        if (selection.kind === "task" && state.graph) {
          (state.graph.groups || []).some(function (group) {
            var task = (group.tasks || []).find(function (candidate) { return candidate.id === selection.task_id; });
            if (!task) return false;
            model.title = task.title;
            model.status = task.status;
            model.fields = [
              { label: "TASK ID", value: task.id },
              { label: "KIND", value: task.kind || "task" },
              { label: "INSTRUCTION", value: task.instruction || "" },
              { label: "GATE", value: task.gate || "" },
              { label: "AGENT", value: task.assignment && (task.assignment.agent_label || task.assignment.agent_id) || "" }
            ];
            return true;
          });
        }
      }
      renderDetailPanels(doc, regions.inspector, model, state.query.panel, {
        onPanelChange: function (panel) {
          state.query.panel = panel;
          syncUrl(true);
          render();
        }
      });
    }

    function renderDiagnosticsRegion() {
      var diagnostics = state.query.mode === "master" && state.master ? (state.master.diagnostics || []) : [];
      var unavailable = state.query.mode === "master" && state.master ? (state.master.unavailable || []) : [];
      renderDiagnosticsRail(doc, regions.diagnostics, diagnostics, unavailable, {
        onSelectDiagnostic: function (diagnostic, index) {
          state.query.sel = "diag:" + index;
          state.query.panel = "diagnostics";
          syncUrl(true);
          render();
        }
      });
    }

    function renderFooter() {
      var footer = regions.footer;
      footer.replaceChildren();
      if (state.query.mode === "master" && state.master) {
        var hashes = ["inventory " + String(state.master.inventory_hash || "—").slice(0, 15),
          state.master.view_hash ? "view " + String(state.master.view_hash).slice(0, 15) : null,
          state.master.truncated ? "TRUNCATED" : null].filter(Boolean).join(" · ");
        footer.append(el(doc, "small", "mg-footer-source", "MASTER · " + hashes));
      } else if (state.graph) {
        footer.append(el(doc, "small", "mg-footer-source",
          "SOURCE · " + (state.graph.source || "") ));
      }
    }

    function showDegraded(errorInfo) {
      state.error = errorInfo;
      announce(errorInfo.title + ". " + errorInfo.message, true);
      render();
    }

    /* ---- state transitions ---- */

    function applySearch(value) {
      state.query.q = String(value || "").slice(0, caps.maxQueryLength);
      syncUrl(true);
      render();
      var count = state.query.mode === "master"
        ? (visibility() || { matchCount: 0 }).matchCount
        : computeIndividualVisibility(collectTasks(), { q: state.query.q, status: state.query.status }).matchCount;
      if (state.query.q) announce(matchSummary(count));
      else announce("Search cleared");
    }

    function collectTasks() {
      var tasks = [];
      ((state.graph && state.graph.groups) || []).forEach(function (group) { tasks = tasks.concat(group.tasks || []); });
      return tasks;
    }

    function toggleStatusToken(token, on) {
      var next = state.query.status.filter(function (candidate) { return candidate !== token; });
      if (on) next.push(token);
      state.query.status = sanitizeStatusTokens(next, state.query.mode);
      syncUrl(true);
      render();
      announce("Status filter: " + (state.query.status.join(", ") || "all"));
    }

    function toggleRelToken(token, on) {
      var next = state.query.rel.filter(function (candidate) { return candidate !== token; });
      if (on) next.push(token);
      state.query.rel = sanitizeRelTokens(next);
      syncUrl(true);
      render();
      announce("Relationship filter: " + (state.query.rel.join(", ") || "all"));
    }

    function select(sel, push) {
      state.query.sel = sel || "";
      if (history) applyQueryState(history, state.query, { push: Boolean(push) });
      render();
      var selection = classifySelection(sel, state.query.mode);
      if (selection.kind !== "none") {
        var model = state.query.mode === "master" ? buildSelectionModel(selection, state.master) : null;
        announce("Selected " + (model && model.title ? model.title : sel));
      }
    }

    function activateEdge(edge) {
      var outcome = resolveCrossLink(edge);
      if (outcome.action === "navigate" && (parseNamespacedId(edge.id) || {}).namespace === "link") {
        state.breadcrumbs = pushBreadcrumb(state.breadcrumbs, {
          from: "master", edge_id: edge.id, return_query: serializeQueryState(state.query)
        });
        state.query.mode = "individual";
        state.query.project = outcome.target.project;
        state.query.sel = outcome.target.sel;
        syncUrl(true);
        announce("Following cross-link to project " + outcome.target.project);
        loadIndividual(outcome.target.project).then(render);
        return;
      }
      // Unresolved / ambiguous / self / internal dep: inspect, never navigate.
      state.query.sel = edge.id;
      if (outcome.action === "inspect" && outcome.reason && outcome.reason !== "missing_edge") {
        state.query.panel = "info";
        announce("Cross-link is " + outcome.reason + "; showing details instead of navigating.");
      }
      syncUrl(true);
      render();
    }

    function goBackToMaster() {
      var popped = popBreadcrumb(state.breadcrumbs);
      state.breadcrumbs = popped.stack;
      if (!popped.entry) return;
      var restored = parseQueryState(popped.entry.return_query);
      restored.sel = popped.entry.edge_id || restored.sel;
      state.query = restored;
      syncUrl(true);
      announce("Back to master view");
      loadMaster().then(render);
    }

    function setMode(mode) {
      if (MODES.indexOf(mode) === -1 || mode === state.query.mode) return;
      state.query.mode = mode;
      state.query.status = sanitizeStatusTokens(state.query.status, mode);
      state.query.sel = "";
      state.error = null;
      syncUrl(true);
      announce(mode === "master" ? "Master view" : "Individual view");
      var pending = mode === "master" ? loadMaster() : loadIndividual(state.query.project);
      render();
      return pending.then(render);
    }

    function selectProject(projectKey) {
      var project = (state.projects && state.projects.projects || []).find(function (candidate) {
        return candidate.project_key === projectKey;
      });
      if (!project || project.available === false) {
        state.switcherOpen = false;
        showDegraded(errorStateFor(project ? "unavailable_project" : "not_in_inventory"));
        return Promise.resolve(null);
      }
      state.query.mode = "individual";
      state.query.project = projectKey;
      state.query.sel = "";
      state.error = null;
      syncUrl(true);
      announce("Project " + projectKey);
      var pending = loadIndividual(projectKey);
      render();
      return pending.then(render);
    }

    /* ---- data loading (GET only; caches per §10.3) ---- */

    function loadProjects(force) {
      return api.projects().then(function (payload) {
        var cached = projectsCache.get(payload && payload.inventory_hash);
        if (!force && cached) { state.projects = cached; return cached; }
        projectsCache.set(payload && payload.inventory_hash, payload);
        state.projects = payload;
        return payload;
      }).catch(function (error) {
        state.projects = state.projects || null;
        state.error = errorStateFor(error.code, error.message);
        return null;
      });
    }

    function loadMaster(force) {
      state.loading = !state.master; // keep previous content visible on refresh (§10.2)
      return api.masterGraph().then(function (payload) {
        var key = masterCacheKey(payload);
        if (!force && key && masterCache.get(key)) payload = masterCache.get(key);
        else masterCache.set(key, payload);
        state.master = payload;
        state.loading = false;
        state.error = null;
        return payload;
      }).catch(function (error) {
        state.loading = false;
        state.error = errorStateFor(error.code === "network" ? "network" : (error.code || "compose_failed"), error.message);
        announce(state.error.title, true);
        return null;
      });
    }

    function loadIndividual(projectKey) {
      state.loading = !state.graph;
      return api.graph(projectKey || "", true).then(function (payload) {
        state.graph = payload;
        state.loading = false;
        state.error = null;
        return payload;
      }).catch(function (error) {
        state.loading = false;
        state.error = errorStateFor(error.code, error.message);
        announce(state.error.title, true);
        return null;
      });
    }

    function refresh() {
      state.error = null;
      projectsCache.invalidate();
      masterCache.invalidate();
      var loads = [loadProjects(true)];
      loads.push(state.query.mode === "master" ? loadMaster(true) : loadIndividual(state.query.project));
      return Promise.all(loads).then(render);
    }

    /* Restore from URL (contract §6.2): parse → fetch → filters → selection
     * → announce. */
    function init() {
      state.query = parseQueryState(getSearch());
      state.loading = true;
      render();
      var loads = [loadProjects(false)];
      loads.push(state.query.mode === "master" ? loadMaster(false) : loadIndividual(state.query.project));
      return Promise.all(loads).then(function () {
        // Drop a stale selection that no longer resolves; keep filters (§6.2).
        if (state.query.sel && state.query.mode === "master" && state.master) {
          var selection = classifySelection(state.query.sel, "master");
          var model = buildSelectionModel(selection, state.master);
          if (selection.kind !== "none" && !model.title && selection.kind !== "diagnostic") {
            state.query.sel = "";
            if (history) applyQueryState(history, state.query, { push: false });
          }
        }
        state.loading = false;
        render();
        announce((state.query.mode === "master" ? "Master view" : "Individual view") + " ready");
        return state;
      });
    }

    return {
      init: init,
      refresh: refresh,
      setMode: setMode,
      selectProject: selectProject,
      select: select,
      applySearch: applySearch,
      searchDebounced: function (value) { debouncedSearch(function () { applySearch(value); }); },
      flushSearch: function () { debouncedSearch.flush(); },
      toggleStatusToken: toggleStatusToken,
      toggleRelToken: toggleRelToken,
      activateEdge: activateEdge,
      goBackToMaster: goBackToMaster,
      getState: function () { return state; },
      regions: regions,
      announcer: announcer,
      render: render
    };
  }

  /* ------------------------------------------------------------------ *
   * Export surface
   * ------------------------------------------------------------------ */

  var MasterGraph = {
    MODES: MODES,
    PANELS: PANELS,
    STATUS_TOKENS: STATUS_TOKENS,
    REL_TOKENS: REL_TOKENS,
    STATUS_LABELS: STATUS_LABELS,
    RENDER_BUDGET: RENDER_BUDGET,
    QUERY_DEFAULTS: QUERY_DEFAULTS,
    ERROR_COPY: ERROR_COPY,
    normalizeText: normalizeText,
    parseQueryState: parseQueryState,
    serializeQueryState: serializeQueryState,
    applyQueryState: applyQueryState,
    effectiveView: effectiveView,
    parseNamespacedId: parseNamespacedId,
    classifySelection: classifySelection,
    createDebouncer: createDebouncer,
    buildIndividualSearchIndex: buildIndividualSearchIndex,
    buildMasterSearchIndex: buildMasterSearchIndex,
    searchMatches: searchMatches,
    matchSummary: matchSummary,
    sanitizeStatusTokens: sanitizeStatusTokens,
    sanitizeRelTokens: sanitizeRelTokens,
    edgeRelToken: edgeRelToken,
    edgeEndpoint: edgeEndpoint,
    masterNodeStatusToken: masterNodeStatusToken,
    computeMasterVisibility: computeMasterVisibility,
    computeIndividualVisibility: computeIndividualVisibility,
    groupNodesByProject: groupNodesByProject,
    planMasterRender: planMasterRender,
    computeListWindow: computeListWindow,
    resolveCrossLink: resolveCrossLink,
    pushBreadcrumb: pushBreadcrumb,
    popBreadcrumb: popBreadcrumb,
    isAbsolutePath: isAbsolutePath,
    sanitizeEvidenceRef: sanitizeEvidenceRef,
    capEvidenceRefs: capEvidenceRefs,
    formatTestEntry: formatTestEntry,
    formatDecisionEntry: formatDecisionEntry,
    emptyPanelCopy: emptyPanelCopy,
    diagnosticCounts: diagnosticCounts,
    diagnosticsForSelection: diagnosticsForSelection,
    errorStateFor: errorStateFor,
    setBusy: setBusy,
    createKeyedCache: createKeyedCache,
    masterCacheKey: masterCacheKey,
    createApiClient: createApiClient,
    handleActivationKey: handleActivationKey,
    attachActivation: attachActivation,
    captureFocusId: captureFocusId,
    restoreFocusById: restoreFocusById,
    createAnnouncer: createAnnouncer,
    projectPrimaryLabel: projectPrimaryLabel,
    projectOptionName: projectOptionName,
    renderProjectSwitcher: renderProjectSwitcher,
    renderModeToggle: renderModeToggle,
    renderSearchControl: renderSearchControl,
    renderStatusFilter: renderStatusFilter,
    renderRelationshipFilter: renderRelationshipFilter,
    masterHeroMetrics: masterHeroMetrics,
    masterNodeAccessibleName: masterNodeAccessibleName,
    edgeAccessibleName: edgeAccessibleName,
    layoutMasterGraph: layoutMasterGraph,
    renderMasterSvg: renderMasterSvg,
    buildMasterListRows: buildMasterListRows,
    renderMasterList: renderMasterList,
    renderDetailPanels: renderDetailPanels,
    buildSelectionModel: buildSelectionModel,
    renderDiagnosticsRail: renderDiagnosticsRail,
    renderDegradedPanel: renderDegradedPanel,
    renderSkeleton: renderSkeleton,
    renderBreadcrumb: renderBreadcrumb,
    createMasterGraphBrowser: createMasterGraphBrowser
  };

  if (typeof module !== "undefined" && module.exports) module.exports = MasterGraph;
  if (globalScope) globalScope.FractalMasterGraph = MasterGraph;

})(typeof window !== "undefined" ? window : (typeof globalThis !== "undefined" ? globalThis : null));
