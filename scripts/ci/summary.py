"""Report outcomes without treating compilation or headless startup as GUI evidence."""

import argparse
import os
from pathlib import Path

from ci_common import TARGETS, save_support


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    args = parser.parse_args()
    lines = [
        f"## Native development build: `{args.target}`",
        "",
        "Unsigned development artifacts; no signing, notarization, or release publishing.",
        "",
        f"- atomic_save_support={save_support(args.target)}",
        "- gui_smoke_executed=false (no hosted GUI/compositor smoke)",
        "- Runtime evidence covers headless CLI/service checks, not interactive desktop behavior.",
    ]
    if save_support(args.target) != "UNSUPPORTED":
        lines.append("- The headless runtime step runs CLI startup, a 100-document lazy-session benchmark, and service save/fault smoke.")
    else:
        lines.extend([
            "- **EXPERIMENTAL — CANNOT SAVE FILES.** Atomic saving is unsupported.",
            "- Save-dependent service smoke is not run; recovery is not a substitute for saving.",
            "- Every packaged artifact must contain EXPERIMENTAL-CANNOT-SAVE.txt.",
        ])
    lines.extend(["", "| Step | Outcome |", "| --- | --- |"])
    for label, variable in (
        ("Native host/toolchain and platform tools", "CI_PREPARE_STATUS"),
        ("All workspace tests", "CI_TEST_STATUS"),
        ("Release workspace build", "CI_BUILD_STATUS"),
        ("Headless binary runtime", "CI_SMOKE_STATUS"),
        ("Machine-header check, SHA-256 integrity, and packaging", "CI_PACKAGE_STATUS"),
        ("14-day artifact upload", "CI_UPLOAD_STATUS"),
    ):
        lines.append(f"| {label} | {os.environ.get(variable, 'not-run')} |")
    lines.extend([
        "",
        "Machine headers check the encoded architecture; SHA-256 records file integrity. "
        "Neither is a signature or cryptographic proof of native execution.",
        "",
    ])
    summary = "\n".join(lines)
    print(summary)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a", encoding="utf-8") as output:
            output.write(summary)


if __name__ == "__main__":
    main()
