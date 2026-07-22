"""Test import shim for the hyphenated execution-graph application directory."""

from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path

SERVER_PATH = Path(__file__).resolve().parents[1] / "server.py"
SPEC = spec_from_file_location("fractal_execution_graph_server", SERVER_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"Could not load {SERVER_PATH}")
MODULE = module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

parse_prd = MODULE.parse_prd
