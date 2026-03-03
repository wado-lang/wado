#!/usr/bin/env python3
"""Compile Wado programs at -O2 and measure wasm binary sizes."""

import json
import os
import subprocess
import sys
import tempfile

WADO = "./target/release/wado"

PROGRAMS = [
    ("hello_world", "wasm-size/hello_world/hello_world.wado"),
    ("pi_approx", "wasm-size/pi_approx/pi_approx.wado"),
    ("zlib", "wasm-size/zlib/zlib.wado"),
]


def measure_size(name: str, src: str) -> dict:
    with tempfile.NamedTemporaryFile(suffix=".wasm", delete=False) as f:
        wasm = f.name
    try:
        subprocess.run(
            [WADO, "compile", "-O2", "-o", wasm, src],
            check=True,
            stderr=subprocess.DEVNULL,
        )
        size = os.path.getsize(wasm)
    finally:
        os.unlink(wasm)
    return {"name": name, "unit": "bytes", "value": size}


results = [measure_size(name, src) for name, src in PROGRAMS]

json.dump(results, sys.stdout, indent=2)
print()
