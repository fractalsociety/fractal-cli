"use strict";

const { normalizeFilter, matches } = require("./filter");
const { keyboardOrder } = require("./keyboard");

class TaskBoard {
  constructor(tasks) {
    this._tasks = Array.isArray(tasks) ? tasks.map((task) => ({ ...task })) : [];
    this._history = [];
  }

  visible(filter) {
    throw new Error("task pending");
  }

  focusOrder(filter, focusedId) {
    throw new Error("task pending");
  }

  applyOptimistic(id, patch) {
    throw new Error("task pending");
  }

  settle(serverTasks) {
    throw new Error("task pending");
  }

  rollback() {
    throw new Error("task pending");
  }
}

module.exports = { TaskBoard };

