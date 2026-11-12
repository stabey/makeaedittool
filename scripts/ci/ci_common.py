"""Small shared contracts for the native hosted-runner jobs (Python 3.11+)."""

import hashlib
from pathlib import Path
import subprocess
import tomllib


ROOT = Path(__file__).resolve().parents[2]
RUST_VERSION = "1.99.1"
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
)


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def binary_paths(target):
    suffix = ".exe" if "windows" in target else ""
    directory = ROOT / "target" / target / "release"
    return {name: directory / f"{name}{suffix}" for name in ("editor", "editor-services")}


def save_support(target):
    if target.endswith("apple-darwin"):
        return "SUPPORTED (macOS)"
    if "windows" in target:
        return "SUPPORTED (Windows)"
    if "linux" in target:
        return "SUPPORTED (Linux)"
    return "UNSUPPORTED"


def pinned_toolchain():
    with (ROOT / "rust-toolchain.toml").open("rb") as source:
        toolchain = tomllib.load(source)["toolchain"]
    if toolchain["channel"] != RUST_VERSION:
        raise RuntimeError("Review the CI host/toolchain contract before changing Rust versions")
    if toolchain["profile"] != "minimal" or set(toolchain["components"]) != {"rustfmt", "clippy"}:
        raise RuntimeError("CI expects the reviewed minimal toolchain with rustfmt and clippy")
    return toolchain


def rust_identity(target):
    pinned_toolchain()
    identity = subprocess.check_output(
        ["rustc", "-vV"], cwd=ROOT, text=True, encoding="utf-8"
    ).strip()
    fields = dict(line.split(": ", 1) for line in identity.splitlines() if ": " in line)
    if fields.get("release") != RUST_VERSION or fields.get("host") != target:
        raise RuntimeError(f"Expected native Rust {RUST_VERSION} host {target}, got:\n{identity}")
    return identity
