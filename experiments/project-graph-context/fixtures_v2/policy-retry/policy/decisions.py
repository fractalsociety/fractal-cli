"""Decoy classification helpers; retry policy owns terminal decisions."""


def classify(outcome):
    if outcome == "ok":
        return "success"
    if outcome == "transient":
        return "retryable"
    return "terminal"

