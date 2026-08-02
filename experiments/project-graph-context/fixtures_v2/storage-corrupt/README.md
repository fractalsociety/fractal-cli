# Corrupt/reload state fixture

Implement `StateStore` in `app/storage.py` using only the standard library.
The JSON document has version `1` and an `entries` object.  `load` must return
an empty store, without raising, for a missing file, malformed JSON, a
non-object document, a wrong version, or an invalid `entries` value.  Valid
entries reload normally.  Expiration is inclusive: a key is absent at the
exact deadline and after it.  Saving uses a sibling temporary file followed by
an atomic replacement and leaves no temporary file behind.

`app/store_helpers.py` is a deliberately plausible decoy.
