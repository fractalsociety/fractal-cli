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
console.log("master graph helper tests passed");
