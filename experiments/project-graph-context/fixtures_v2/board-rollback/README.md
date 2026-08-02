# Optimistic task-board fixture

Implement `lib/board.js` without dependencies or network access.  It shares
the filter and keyboard semantics of the sibling board fixture.  Calling
`applyOptimistic(id, patch)` immediately changes that task while recording one
deep snapshot.  `settle(serverTasks)` accepts a complete server snapshot,
clears pending history, and preserves server order.  `rollback()` restores the
pre-optimistic snapshot exactly (including order and fields), is idempotent,
and is a no-op for an unknown id.  A failed server response is represented by
the caller invoking `rollback`, not by making a request here.

`server_stub.js` only parses already-provided text and must not be used for
network access.
