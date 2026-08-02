# Task-board filtering fixture

Implement the missing methods in `lib/board.js` using Node's standard library
only.  `visible(filter)` preserves the original task order and accepts status
(`all`, `todo`, `doing`, `done`), a case-insensitive trimmed title/id query,
and an exact assignee.  Unknown status behaves like `all`; malformed filters
are harmless.  `focusOrder(filter, focusedId)` returns visible ids in stable
keyboard order, starting at the focused id when it is visible and wrapping to
the beginning.  A missing focus starts at the first item.

Do not use `server_stub.js`; it is an intentionally plausible decoy.
