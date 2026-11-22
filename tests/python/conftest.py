"""Ensure `poc/python` is importable even without editable installation."""

from pathlib import Path
import sys

POC_PYTHON_ROOT = Path(__file__).resolve().parents[2] / "poc" / "python"
if str(POC_PYTHON_ROOT) not in sys.path:
    sys.path.insert(0, str(POC_PYTHON_ROOT))
