"use strict";

// A network-looking decoy.  The benchmark forbids network calls; this parser
// is intentionally pure and is not part of TaskBoard's filtering contract.
function parseServerEnvelope(text) {
  return JSON.parse(text);
}

module.exports = { parseServerEnvelope };

