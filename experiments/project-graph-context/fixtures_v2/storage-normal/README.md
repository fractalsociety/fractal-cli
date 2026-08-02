# Durable state fixture

Implement `StateStore` in `app/storage.py`.  A store writes one versioned JSON
document at its configured path, reloads it, and treats an entry as expired
when the clock is at or beyond its absolute deadline.  `put(key, value,
ttl=None)` stores a value; a non-negative TTL is relative to the current clock.
`get` returns the supplied default for a missing or expired key.  Save should
be crash-safe (write a sibling temporary file then replace the target) and
should not mutate unrelated files.

The clock and codec modules are intentionally siblings; do not implement the
store in `store_helpers.py`.
