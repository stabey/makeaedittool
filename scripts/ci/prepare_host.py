"""Assert the real host, install the pinned native Rust, and prepare GPUI tools."""

import argparse
import os
from pathlib import Path
import platform
import re
import subprocess

from ci_common import ROOT, TARGETS, pinned_toolchain, rust_identity


LINUX_PACKAGES = (
    "clang", "cmake", "ninja-build", "pkg-config", "libssl-dev", "libzstd-dev",
    "libfontconfig1-dev", "libfreetype-dev", "libwayland-dev", "libxkbcommon-dev",
    "libxkbcommon-x11-dev", "libx11-dev", "libx11-xcb-dev", "libxcb1-dev",
    "libxcb-render0-dev", "libxcb-shape0-dev", "libxcb-xfixes0-dev",
    "libxcb-randr0-dev", "libxcb-shm0-dev", "libvulkan-dev", "libasound2-dev",
)


def export_environment(name, value):
    os.environ[name] = value
    if os.environ.get("GITHUB_ENV"):
        with Path(os.environ["GITHUB_ENV"]).open("a", encoding="utf-8") as output:
            output.write(f"{name}={value}\n")
    print(f"{name}={value}", flush=True)


def assert_native_host(target):
    expected_os = "Windows" if "windows" in target else "Darwin" if "apple" in target else "Linux"
    if platform.system() != expected_os:
        raise RuntimeError(f"Target {target} requires {expected_os}, not {platform.system()}")
    if expected_os == "Windows":
        actual = subprocess.check_output(
            ["pwsh", "-NoLogo", "-NoProfile", "-Command",
             "[System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()"],
            text=True, encoding="utf-8",
        ).strip()
        expected = "Arm64" if target.startswith("aarch64") else "X64"
    else:
        actual = subprocess.check_output(["uname", "-m"], text=True, encoding="utf-8").strip()
        expected = "arm64" if target.startswith("aarch64") else "x86_64"
        if expected_os == "Darwin":
            translated = subprocess.run(
                ["sysctl", "-n", "sysctl.proc_translated"],
                capture_output=True, text=True, encoding="utf-8",
            )
            # Intel Macs do not expose this sysctl. Rosetta reports 1.
            if translated.stdout.strip() == "1":
                raise RuntimeError("Rosetta is not a native CI host")
            if translated.returncode and expected == "arm64":
                raise RuntimeError(f"Cannot establish native Apple Silicon execution: {translated.stderr}")
    if actual != expected:
        raise RuntimeError(f"Host OS architecture {actual!r} does not match {target} ({expected})")
    print(f"native_os={expected_os} native_os_architecture={actual}", flush=True)


def prepare_windows(target):
    program_files = Path(os.environ["ProgramFiles(x86)"])
    vswhere = program_files / "Microsoft Visual Studio" / "Installer" / "vswhere.exe"
    arch = "arm64" if target.startswith("aarch64") else "x64"
    component = "Microsoft.VisualStudio.Component.VC.Tools." + (
        "ARM64" if arch == "arm64" else "x86.x64"
    )
    installation = subprocess.check_output(
        [str(vswhere), "-latest", "-products", "*", "-requires", component,
         "-property", "installationPath", "-utf8"],
        text=True, encoding="utf-8",
    ).strip()
    if not installation or not Path(installation).is_dir():
        raise RuntimeError(f"No Visual Studio installation with {component}")
    print(f"visual_studio={installation} required_component={component}", flush=True)
    # Do not call bare VsDevCmd: its x86 defaults can poison rustc/cc-rs discovery.
    sdk_bin = program_files / "Windows Kits" / "10" / "bin"
    candidates = [
        version / arch / "fxc.exe"
        for version in sdk_bin.iterdir()
        if re.fullmatch(r"\d+(?:\.\d+)+", version.name)
        and (version / arch / "fxc.exe").is_file()
    ]
    if not candidates:
        raise RuntimeError(f"No existing native {arch}/fxc.exe under {sdk_bin}; no emulated fallback")
    compiler = max(candidates, key=lambda path: tuple(map(int, path.parent.parent.name.split("."))))
    export_environment("GPUI_FXC_PATH", str(compiler))


def prepare_macos():
    commands = [["xcrun", "-sdk", "macosx", tool, "--version"] for tool in ("metal", "metallib")]
    probes = [subprocess.run(command, check=False) for command in commands]
    if any(probe.returncode != 0 for probe in probes):
        subprocess.run(["xcodebuild", "-downloadComponent", "MetalToolchain"], check=True)
        for command in commands:
            subprocess.run(command, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--toolchain-only", action="store_true", help="Skip GPUI tools in the fmt job")
    args = parser.parse_args()
    assert_native_host(args.target)  # Must precede every Rust invocation.
    toolchain = pinned_toolchain()
    host_toolchain = f"{toolchain['channel']}-{args.target}"
    subprocess.run(["rustup", "set", "default-host", args.target], check=True)
    command = ["rustup", "toolchain", "install", host_toolchain, "--profile", toolchain["profile"]]
    for component in toolchain["components"]:
        command.extend(["--component", component])
    subprocess.run([*command, "--no-self-update"], cwd=ROOT, check=True)
    export_environment("RUSTUP_TOOLCHAIN", host_toolchain)
    print(rust_identity(args.target), flush=True)
    if not args.toolchain_only:
        if "windows" in args.target:
            prepare_windows(args.target)
        elif "apple" in args.target:
            prepare_macos()
        else:
            subprocess.run(["sudo", "apt-get", "update"], check=True)
            subprocess.run(
                ["sudo", "apt-get", "install", "-y", "--no-install-recommends", *LINUX_PACKAGES],
                check=True,
            )


if __name__ == "__main__":
    main()
