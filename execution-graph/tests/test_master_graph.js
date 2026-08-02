"use strict";

/* Focused, dependency-free regression checks for the master browser helpers.
 * Run with: node execution-graph/tests/test_master_graph.js */
const assert = require("assert");
const Graph = require("../master-graph.js");

const projects = [
  { project_key: "fractal-cli-bbbfd315b970", labels: ["fractal-cli"], available: true, catalog_state: "valid" },
  { project_key: "link-1785676949627-7ca5f32174ac", labels: ["link"], available: true, catalog_state: "valid" },
  { project_key: "standalone-aaaaaaaaaa", labels: ["standalone"], available: true, catalog_state: "valid" }
];
const nodes = projects.flatMap((project) => [
  { id: `component:${project.project_key}/core`, kind: "component", project_key: project.project_key, title: `${project.labels[0]} core` },
  { id: `capability:${project.project_key}/feature`, kind: "capability", project_key: project.project_key, title: `${project.labels[0]} feature` }
]);
const view = {
  projects,
  nodes,
  edges: [{
    id: "link:link-1785676949627-7ca5f32174ac/project-ref",
    type: "related_to",
    resolution: "resolved",
    origin_project_key: "link-1785676949627-7ca5f32174ac",
    from: "project:link-1785676949627-7ca5f32174ac",
    to: { node_id: "project:fractal-cli-bbbfd315b970" }
  }]
};

const groups = Graph.buildMasterGroups(view);
const ecosystem = groups.find((group) => group.title === "Fractal ecosystem");
assert(ecosystem, "resolved cross-project link creates an ecosystem group");
assert.deepStrictEqual(ecosystem.projectKeys, [
  "fractal-cli-bbbfd315b970",
  "link-1785676949627-7ca5f32174ac"
]);
assert(groups.some((group) => group.title === "Standalone projects"));
assert(ecosystem.categoryCounts.component === 2 && ecosystem.categoryCounts.capability === 2);

const query = Graph.parseQueryState("?mode=master&view=list&q=core&panel=tests");
assert.strictEqual(query.mode, "master");
assert.strictEqual(query.view, "list");
assert.strictEqual(Graph.serializeQueryState(query), "?mode=master&view=list&q=core&panel=tests");
assert.strictEqual(Graph.effectiveView(Graph.parseQueryState("?mode=master"), 1440), "graph");
assert.strictEqual(Graph.effectiveView(Graph.parseQueryState("?mode=master"), 560), "list");

const visibility = Graph.computeMasterVisibility(view, { q: "fractal-cli", status: [], rel: [] });
assert(visibility.matchCount > 0 && visibility.matchIds["component:fractal-cli-bbbfd315b970/core"]);
const plan = Graph.planMasterRender({
  ...view,
  nodes: Array.from({ length: 401 }, (_, index) => ({
    id: `component:fractal-cli-bbbfd315b970/node-${index}`,
    kind: "component",
    project_key: "fractal-cli-bbbfd315b970",
    title: `Node ${index}`
  }))
}, { caps: Graph.RENDER_BUDGET });
assert(plan.nodes.length <= Graph.RENDER_BUDGET.maxSvgNodes);
assert(plan.diagnostics.some((diagnostic) => diagnostic.code === "graph_truncated"));

const windowPlan = Graph.computeListWindow(2577, 1288, Graph.RENDER_BUDGET);
assert(windowPlan.virtualized && windowPlan.end - windowPlan.start <= Graph.RENDER_BUDGET.rowOverscan * 2 + 1);

assert.strictEqual(Graph.clampMasterZoom(0.1), 0.5);
assert.strictEqual(Graph.clampMasterZoom(4), 2);
assert.strictEqual(Graph.masterZoomLabel(1.25), "130%");

/* Escape/focus must not call focus() on an opener detached from the document
 * (a common state during mode swaps and in DOM harnesses). */
let detachedFocusCalls = 0;
const detached = { focus: () => { detachedFocusCalls += 1; } };
const detachedDoc = { documentElement: { contains: () => false } };
assert.strictEqual(Graph.isConnectedElement(detachedDoc, detached), false);
assert.strictEqual(Graph.focusIfConnected(detachedDoc, detached), false);
assert.strictEqual(detachedFocusCalls, 0);

const explicitPlan = Graph.planMasterRender(view, {
  caps: Graph.RENDER_BUDGET,
  forceHierarchy: true,
  visibility
});
assert(explicitPlan.nodes.some((node) => node.id === "component:fractal-cli-bbbfd315b970/core"),
  "search should reveal a matching node through hierarchy");

const failureView = {
  records: [
    { id: "failure:n1:tool_failure", node_id: "n1", failure_code: "tool_failure", state: "resolved",
      summary: "compiler failed", component: "compiler", observations: [{ attempt: 1, outcome: "failed", summary: "first" }],
      resolution: { success: true, summary: "retry passed", evidence: [{ sha256: "sha256:" + "a".repeat(64) }] } },
    { id: "failure:n2:timeout", node_id: "n2", failure_code: "timeout", state: "unresolved", summary: "timed out" }
  ],
  lessons: [{ id: "lesson:compiler", summary: "Pin compiler version", status: "adopted", component: "compiler" }],
  edges: [{ id: "edge:lesson_from", type: "lesson_from", from: "failure:n1:tool_failure", to: "lesson:compiler" }]
};
assert.strictEqual(Graph.filterFailureRecords(failureView.records, { state: ["resolved"], lesson: "compiler" }, failureView).length, 1);
assert.strictEqual(Graph.searchFailureRecords(failureView.records, "timed", failureView).count, 1);
assert.deepStrictEqual(Graph.failureTimeline(failureView.records[0]).map((entry) => entry.kind), ["observation", "resolution"]);
assert.strictEqual(Graph.failurePath(failureView, "failure:n1:tool_failure", "lesson:compiler").length, 1);
const bounded = Graph.boundedFailureRecords(failureView.records, 1);
assert.strictEqual(bounded.records.length, 1);
assert.strictEqual(bounded.hiddenCount, 1);
const failureQuery = Graph.parseQueryState("?failure_panel=1&failure_sel=f1&failure_state=resolved&failure_query=compiler");
assert.strictEqual(failureQuery.failurePanel, true);
assert.strictEqual(failureQuery.failureQuery, "compiler");
assert(Graph.serializeQueryState(failureQuery).includes("failure_state=resolved"));
console.log("master graph helper tests passed");
