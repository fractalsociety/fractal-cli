"use strict";

function normalizeFilter(filter) {
  const value = filter && typeof filter === "object" ? filter : {};
  return {
    status: typeof value.status === "string" ? value.status.toLowerCase() : "all",
    query: typeof value.query === "string" ? value.query.trim().toLowerCase() : "",
    assignee: typeof value.assignee === "string" ? value.assignee : "",
  };
}

function matches(task, filter) {
  const normalized = normalizeFilter(filter);
  if (normalized.status !== "all" && task.status !== normalized.status) return false;
  if (normalized.assignee && task.assignee !== normalized.assignee) return false;
  if (normalized.query) {
    const haystack = `${task.id} ${task.title || ""}`.toLowerCase();
    if (!haystack.includes(normalized.query)) return false;
  }
  return true;
}

module.exports = { normalizeFilter, matches };

