"""A deliberately over-permissive decoy policy; do not call it."""


def should_retry(_outcome):
    return True

