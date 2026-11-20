"""
jsonbit Python Production SDK — Powered by the Pure-Rust Bit-Packed Columnar & BitVec64 Engine.
Zero external pip dependencies required (uses Python stdlib subprocess + json).
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Dict, List, Optional, Union


def _resolve_default_bin() -> str:
    exe = "jsonbit.exe" if os.name == "nt" else "jsonbit"
    env_bin = os.environ.get("JSONBIT_BIN")
    if env_bin and os.path.exists(env_bin):
        return env_bin
    # 1. Local Cargo release build (when running from the repository root)
    local_bin = Path(__file__).resolve().parent.parent.parent / "target" / "release" / exe
    if local_bin.exists():
        return str(local_bin)
    # 2. Binary installed alongside the active Python environment (pip / uv / venv)
    venv_bin = Path(sys.executable).resolve().parent / exe
    if venv_bin.exists():
        return str(venv_bin)
    # 3. Global PATH lookup
    path_bin = shutil.which("jsonbit")
    if path_bin:
        return path_bin
    return exe


DEFAULT_BIN = _resolve_default_bin()


class JsonBit:
    """
    Production Python client for the Pure-Rust `jsonbit` Bit-Packed Columnar Engine.
    """

    def __init__(self, data_or_path: Union[str, Path, Dict[str, Any], List[Any]], bin_path: str = DEFAULT_BIN):
        self.bin_path = bin_path
        self._temp_file: Optional[str] = None

        if isinstance(data_or_path, (str, Path)) and os.path.exists(str(data_or_path)):
            self.file_path = str(data_or_path)
        else:
            fd, tmp = tempfile.mkstemp(suffix=".json", prefix="jsonbit_")
            os.close(fd)
            with open(tmp, "w", encoding="utf-8") as f:
                if isinstance(data_or_path, str):
                    f.write(data_or_path)
                else:
                    json.dump(data_or_path, f)
            self._temp_file = tmp
            self.file_path = tmp

    def _run(self, *args: str) -> str:
        res = subprocess.run(
            [self.bin_path, *args],
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
        if res.returncode != 0:
            msg = (res.stderr or res.stdout or f"jsonbit exited with status {res.returncode}").strip()
            raise RuntimeError(msg)
        return res.stdout.strip()

    def card(self) -> str:
        """Returns the ~250-token O(1) Semantic Cube Card."""
        return self._run("card", self.file_path)

    def digest(self) -> str:
        """Returns the 1-Turn Factorized 3NF Holistic Digest (100% of rows + pivot legend)."""
        return self._run("digest", self.file_path)

    def ask(self, question: str) -> str:
        """Executes a natural-language query on BitVec64 indices in microseconds and returns the Verified Slice."""
        return self._run("ask", self.file_path, question)

    def answer(self, question: str) -> str:
        """Extracts the exact scalar answer string for a natural-language question."""
        return self._run("answer", self.file_path, question)

    def sql(self, query: str) -> Any:
        """Executes a read-only SQL SELECT query directly against the Rust Bit-Packed Columnar store."""
        out = self._run("sql", self.file_path, query)
        return json.loads(out)

    def pack(self, out_jsonbit_path: Union[str, Path]) -> str:
        """Compiles the JSON/JSONL dataset into a bit-packed `.jsonbit` binary file on disk."""
        return self._run("pack", self.file_path, str(out_jsonbit_path))

    def rehydrate(self) -> Any:
        """Reconstructs the exact original nested JSON document from the 3NF bit-packed columns."""
        out = self._run("rehydrate", self.file_path)
        return json.loads(out)

    def close(self) -> None:
        if self._temp_file and os.path.exists(self._temp_file):
            os.remove(self._temp_file)
            self._temp_file = None

    def __enter__(self) -> "JsonBit":
        return self

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self.close()


__all__ = ["JsonBit", "DEFAULT_BIN"]
