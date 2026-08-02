"use strict";

function keyboardOrder(tasks, focusedId) {
  const ids = tasks.map((task) => task.id);
  if (!ids.length) return [];
  const index = focusedId == null ? -1 : ids.indexOf(focusedId);
  const start = index >= 0 ? index : 0;
  return ids.slice(start).concat(ids.slice(0, start));
}

module.exports = { keyboardOrder };

