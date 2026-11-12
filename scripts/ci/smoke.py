"""Run real release CLI entrypoints without starting the editor's GUI."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from ci_common import ROOT, TARGETS, binary_paths, rust_identity, save_support, sha256


def run_cli(binary, *arguments):
    print(f"Running {binary.name} {' '.join(map(str, arguments))}", flush=True)
    result = subprocess.run(
        [str(binary), *map(str, arguments)], cwd=ROOT, stdout=subprocess.PIPE,
        text=True, encoding="utf-8", timeout=300,
    )
    print(result.stdout, end="", flush=True)
    result.check_returncode()
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--temp-root", type=Path, default=os.environ.get("RUNNER_TEMP"))
    parser.add_argument("--report", type=Path, default=os.environ.get("CI_SMOKE_REPORT"))
    args = parser.parse_args()
    if args.temp_root is None or args.report is None:
        parser.error("provide --temp-root and --report (or RUNNER_TEMP and CI_SMOKE_REPORT)")
    temp_root = args.temp_root.resolve(strict=True)
    if not temp_root.is_dir() or temp_root.is_relative_to(ROOT):
        parser.error("smoke data must be in a temporary directory outside the workspace")
    args.report.unlink(missing_ok=True)
    print(rust_identity(args.target), flush=True)
    binaries = binary_paths(args.target)
    hashes = {name: sha256(path) for name, path in binaries.items()}
    version = run_cli(binaries["editor"], "--version").strip()
    help_text = run_cli(binaries["editor"], "--help")
    if not version.startswith("editor ") or not help_text.strip():
        raise RuntimeError("Editor version/help output was empty or unexpected")
    with tempfile.TemporaryDirectory(prefix="makeaedittool-ci-", dir=temp_root) as scratch:
        directory = Path(scratch)
        session = run_cli(binaries["editor-services"], "session-bench", directory / "session", "100")
        metrics = [
            dict(token.split("=", 1) for token in line.split() if "=" in token)
            for line in session.splitlines() if line.startswith("metric=lazy_session ")
        ]
        if len(metrics) != 1 or metrics[0].get("tabs") != "100" or metrics[0].get("hydrated_documents") != "1":
            raise RuntimeError("Expected metric=lazy_session tabs=100 with hydrated_documents=1")
        services_smoke = "not-run: unsupported atomic saving on this platform"
        if save_support(args.target) != "UNSUPPORTED":
            output = run_cli(binaries["editor-services"], "smoke", directory / "services")
            if not any(line.startswith("smoke_passed ") for line in output.splitlines()):
                raise RuntimeError("Missing successful service smoke marker")
            services_smoke = "success"
    # Only a fully successful run writes a report. Packaging binds it to these binaries.
    report = {
        "target": args.target,
        "status": "success",
        "editor_version_status": "success",
        "editor_help_status": "success",
        "editor_version": version,
        "session_bench_status": "success",
        "session_bench": metrics[0],
        "services_smoke": services_smoke,
        "atomic_save_support": save_support(args.target),
        "gui_smoke_executed": False,
        "binary_sha256": hashes,
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
