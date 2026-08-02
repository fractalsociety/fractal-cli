"use strict";

function parseServerEnvelope(text) {
  return JSON.parse(text);
}

module.exports = { parseServerEnvelope };

