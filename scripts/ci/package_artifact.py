"""Package checked native binaries, target-resolved notices, and honest build identity."""

import argparse
import csv
from datetime import datetime, timezone
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import struct
import subprocess
import zipfile

from ci_common import ROOT, RUST_VERSION, TARGETS, binary_paths, rust_identity, save_support, sha256


NOTICE_PREFIXES = (
    "LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT", "UNLICENSE",
    "ACKNOWLEDGEMENTS", "ACKNOWLEDGMENTS", "PATENTS",
)


def machine_header(path, target):
    """Check the encoded machine, not cryptographic proof of native execution."""
    with path.open("rb") as binary:
        header = binary.read(64)
        if len(header) != 64:
            raise RuntimeError(f"Empty or truncated executable: {path}")
        if target.endswith("apple-darwin"):
            cpu = 0x0100000C if target.startswith("aarch64") else 0x01000007
            if header[:4] != b"\xcf\xfa\xed\xfe" or struct.unpack_from("<I", header, 4)[0] != cpu:
                raise RuntimeError(f"Wrong Mach-O machine header for {target}: {path}")
            if struct.unpack_from("<I", header, 12)[0] != 2:
                raise RuntimeError(f"Mach-O is not MH_EXECUTE: {path}")
            return f"Mach-O 64-bit cputype=0x{cpu:08x}"
        if "windows" in target:
            if header[:2] != b"MZ":
                raise RuntimeError(f"Missing DOS/PE header: {path}")
            offset = struct.unpack_from("<I", header, 60)[0]
            if offset < 64:
                raise RuntimeError(f"Invalid PE header offset: {path}")
            binary.seek(offset)
            pe = binary.read(6)
            machine = 0xAA64 if target.startswith("aarch64") else 0x8664
            if len(pe) != 6 or pe[:4] != b"PE\0\0" or struct.unpack_from("<H", pe, 4)[0] != machine:
                raise RuntimeError(f"Wrong PE machine header for {target}: {path}")
            return f"PE Machine=0x{machine:04x}"
        if header[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", header, 18)[0] != 62:
            raise RuntimeError(f"Wrong ELF64 little-endian x86_64 machine header: {path}")
        return "ELF64 little-endian e_machine=62 (x86_64)"


def resolved_dependencies(metadata):
    # --filter-platform filters resolve, NOT the packages array. Walk only its graph.
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])
    pending = list(members)
    visited = set()
    while pending:
        package_id = pending.pop()
        if package_id not in visited:
            visited.add(package_id)
            pending.extend(nodes[package_id]["dependencies"])
    dependencies = visited - members
    return sorted(
        (package for package in metadata["packages"] if package["id"] in dependencies),
        key=lambda package: (package["name"], package["version"], package["id"]),
    )


def notice_files(directory):
    files = {}
    if directory.is_dir():
        for entry in sorted(directory.iterdir()):
            if entry.name.upper().startswith(NOTICE_PREFIXES):
                candidates = [entry] if entry.is_file() else sorted(entry.rglob("*"))
                for candidate in candidates:
                    if candidate.is_file():
                        files[candidate.relative_to(directory).as_posix()] = candidate
    return files


def crate_notices(package):
    directory = Path(package["manifest_path"]).parent.resolve()
    files = notice_files(directory)
    declared = package.get("license_file")
    if declared:
        path = (directory / declared).resolve()
        if not path.is_file():
            raise RuntimeError(f"Declared license file missing for {package['id']}: {declared}")
        # Git/path dependencies can declare a license outside their crate directory.
        relative = path.relative_to(directory).as_posix() if path.is_relative_to(directory) else f"declared/{path.name}"
        if relative in files and files[relative].resolve() != path:
            raise RuntimeError(f"Conflicting declared license path for {package['id']}: {relative}")
        files[relative] = path
    expression = package.get("license")
    if not expression:
        if not files:
            raise RuntimeError(f"No license expression or license files for {package['id']}")
        expression = f"SEE LICENSE FILE ({'; '.join(sorted(files))})"
    return expression, files


def add_file(files, name, path):
    entry = PurePosixPath(name)
    if entry.is_absolute() or ".." in entry.parts or "\\" in name or name in files:
        raise RuntimeError(f"Unsafe or duplicate zip entry: {name}")
    if not path.is_file():
        raise RuntimeError(f"Required package file missing: {path}")
    files[name] = path


def license_inventory(metadata, target, files):
    records = []
    crate_directories = set()
    for package in resolved_dependencies(metadata):
        directory = f"licenses/crates/{package['name']}-{package['version']}"
        if directory in crate_directories:
            raise RuntimeError(f"Multiple dependency sources would overwrite {directory}")
        crate_directories.add(directory)
        expression, notices = crate_notices(package)
        names = []
        for relative, path in sorted(notices.items()):
            name = f"{directory}/{relative}"
            add_file(files, name, path)
            names.append(name)
        records.append({
            "package": package["name"], "version": package["version"],
            "license_expression": expression, "repository": package.get("repository") or "",
            "source": package.get("source") or "path dependency", "license_files": names,
            "package_id": package["id"],
        })
    output = io.StringIO(newline="")
    fields = ["package", "version", "license_expression", "repository", "source", "license_files"]
    writer = csv.DictWriter(output, fieldnames=fields, lineterminator="\n", extrasaction="ignore")
    writer.writeheader()
    for record in records:
        writer.writerow({**record, "license_files": "; ".join(record["license_files"])})
    manifest = {
        "target": target,
        "scope": "Resolved default-workspace dependencies for this target, including build/test dependencies; not a binary symbol inventory. The excluded nsh spike is not included.",
        "packages": records,
    }
    return output.getvalue(), manifest


def write_archive(archive, files, generated):
    partial = archive.with_suffix(".zip.partial")
    try:
        # Cargo registry notices may be dated 1970; ZIP's DOS dates start in 1980.
        # Clamp only unrepresentable timestamps, retaining bytes and file modes.
        with zipfile.ZipFile(
            partial, "w", compression=zipfile.ZIP_DEFLATED,
            compresslevel=6, strict_timestamps=False,
        ) as bundle:
            for name, path in sorted(files.items()):
                bundle.write(path, arcname=name)
            for name, text in sorted(generated.items()):
                if name in files:
                    raise RuntimeError(f"Generated notice would overwrite {name}")
                bundle.writestr(name, text)
        partial.replace(archive)
    finally:
        partial.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--commit", default=os.environ.get("BUILD_COMMIT"))
    parser.add_argument("--test-status", default=os.environ.get("CI_TEST_STATUS"))
    parser.add_argument("--build-status", default=os.environ.get("CI_BUILD_STATUS"))
    parser.add_argument("--smoke-report", type=Path, default=os.environ.get("CI_SMOKE_REPORT"))
    parser.add_argument("--output-dir", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    if not args.commit or not re.fullmatch(r"[0-9a-fA-F]{40}", args.commit):
        parser.error("--commit / BUILD_COMMIT must identify the checked-out 40-digit Git commit")
    if args.test_status != "success" or args.build_status != "success":
        parser.error("packaging requires successful workspace tests and release build outcomes")
    if args.smoke_report is None:
        parser.error("--smoke-report / CI_SMOKE_REPORT is required")
    print(f"free_disk_bytes_before_packaging={shutil.disk_usage(ROOT).free}", flush=True)
    rustc = rust_identity(args.target)
    binaries = binary_paths(args.target)
    machines = {name: machine_header(path, args.target) for name, path in binaries.items()}
    hashes = {name: sha256(path) for name, path in binaries.items()}
    smoke = json.loads(args.smoke_report.read_text(encoding="utf-8"))
    if (
        smoke.get("target") != args.target or smoke.get("status") != "success"
        or smoke.get("binary_sha256") != hashes or smoke.get("gui_smoke_executed") is not False
        or smoke.get("atomic_save_support") != save_support(args.target)
        or any(smoke.get(key) != "success" for key in ("editor_version_status", "editor_help_status", "session_bench_status"))
        or smoke.get("session_bench", {}).get("tabs") != "100"
        or smoke.get("session_bench", {}).get("hydrated_documents") != "1"
        or (args.target.endswith("apple-darwin") and smoke.get("macos_services_smoke") != "success")
    ):
        raise RuntimeError("Missing or mismatched headless smoke evidence for these binaries")
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", args.target],
        cwd=ROOT, text=True, encoding="utf-8",
    ))
    files = {}
    for path in binaries.values():
        add_file(files, path.name, path)
    for name in ("LICENSE", "README.md", "docs/KNOWN_LIMITATIONS.md"):
        add_file(files, name, ROOT / name)
    supplemental = sorted(path for path in (ROOT / "docs" / "licenses").rglob("*") if path.is_file())
    if not supplemental:
        raise RuntimeError("Required supplemental docs/licenses notices are missing")
    for path in supplemental:
        add_file(files, path.relative_to(ROOT).as_posix(), path)
    csv_text, manifest = license_inventory(metadata, args.target, files)
    sysroot = Path(subprocess.check_output(
        ["rustc", "--print", "sysroot"], cwd=ROOT, text=True, encoding="utf-8",
    ).strip())
    rust_notices = notice_files(sysroot / "share" / "doc" / "rust")
    for name, path in rust_notices.items():
        add_file(files, f"licenses/rust-toolchain/{name}", path)
    manifest["rust_standard_library"] = {
        "version": RUST_VERSION, "license_expression": "MIT OR Apache-2.0",
        "repository": "https://github.com/rust-lang/rust",
        "source": f"https://github.com/rust-lang/rust/tree/{RUST_VERSION}/library",
        "license_files": [f"licenses/rust-toolchain/{name}" for name in sorted(rust_notices)],
        "installed_sysroot_notices": "bundled" if rust_notices else "unavailable in installed minimal sysroot",
    }
    status = (
        f"target={args.target}\ncommit={args.commit}\n"
        "build_status=SUCCESS (workspace tests, release build, and headless CLI checks)\n"
        f"atomic_save_support={save_support(args.target)}\ngui_smoke_executed=false\n"
        f"macos_services_smoke={smoke['macos_services_smoke']}\n"
    )
    disclaimer = (
        "UNSIGNED DEVELOPMENT BUILD — NOT A PRODUCTION RELEASE\n\n" + status + "\n"
        "No signing, notarization, installer, or automatic update is provided.\n"
        "Hosted runtime evidence covers --help, --version and session-bench 100 only,\n"
        "plus headless save/fault service smoke on macOS. No GUI smoke was executed.\n"
        "CLI startup does not establish interactive, IME, GPU, or desktop usability.\n"
        "Machine-header checks identify the encoded architecture. SHA-256 records\n"
        "file integrity, not a signature or cryptographic proof of native execution.\n"
        "See docs/KNOWN_LIMITATIONS.md and licenses/manifest.json.\n"
    )
    cannot_save = (
        "EXPERIMENTAL — CANNOT SAVE FILES\n\n"
        "atomic_save_support=UNSUPPORTED\ngui_smoke_executed=false\n\n"
        "This Windows/Linux build cannot save user files: the current atomic-save\n"
        "implementation returns Unsupported outside macOS. Do not use it for work\n"
        "you need to save. SQLite draft recovery is NOT a saved user file.\n"
        "Passing workspace tests and headless CLI startup do not change this limit.\n"
        "Save-dependent service smoke and hosted GUI smoke were not run.\n"
        "This is an unsigned experimental development artifact, not a supported release.\n"
    )
    identity = (
        status
        + f"created_utc={datetime.now(timezone.utc).isoformat()}\n"
        + f"ci_run_url={os.environ.get('CI_RUN_URL', 'local; no hosted run asserted')}\n"
        + f"cargo_lock_sha256={sha256(ROOT / 'Cargo.lock')}\n"
        + "workspace_tests=cargo test --locked --workspace --target " + args.target + "\n"
        + "release_build=cargo build --locked --release --workspace --target " + args.target + "\n"
        + "architecture_check=machine-header check with SHA-256 integrity; not cryptographic native-execution proof\n"
        + "rust_standard_library_license=MIT OR Apache-2.0\n"
        + f"rust_toolchain_notices={manifest['rust_standard_library']['installed_sysroot_notices']}\n"
        + "artifact_kind=unsigned development build\n"
        + "runtime_evidence=HEADLESS_SMOKE.json; no hosted GUI validation\n"
        + "\nrustc_verbose_version:\n" + rustc + "\n\nbinaries:\n"
        + "".join(f"{binaries[name].name} machine_header={machines[name]} sha256={hashes[name]}\n" for name in binaries)
    )
    generated = {
        "DEPENDENCY_LICENSES.csv": csv_text,
        "licenses/manifest.json": json.dumps(manifest, indent=2) + "\n",
        "BUILD_IDENTITY.txt": identity,
        "UNSIGNED-DEVELOPMENT-BUILD.txt": disclaimer,
        "HEADLESS_SMOKE.json": json.dumps(smoke, indent=2) + "\n",
        "SHA256SUMS": "".join(f"{hashes[name]}  {binaries[name].name}\n" for name in binaries),
    }
    if not args.target.endswith("apple-darwin"):
        generated["EXPERIMENTAL-CANNOT-SAVE.txt"] = cannot_save
        generated["UNSIGNED-DEVELOPMENT-BUILD.txt"] += "\n" + cannot_save
        if not generated.get("EXPERIMENTAL-CANNOT-SAVE.txt", "").strip():
            raise RuntimeError("Non-macOS packages must contain a non-empty CANNOT-SAVE notice")
    editor = next(package for package in metadata["packages"] if package["id"] in metadata["workspace_members"] and package["name"] == "editor")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    archive = args.output_dir / f"makeaedittool-{editor['version']}-{args.target}-{args.commit[:12]}-unsigned-dev.zip"
    write_archive(archive, files, generated)
    print(f"archive={archive}\narchive_sha256={sha256(archive)}", flush=True)
    if os.environ.get("GITHUB_OUTPUT"):
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
            output.write(f"archive={archive.resolve().as_posix()}\n")


if __name__ == "__main__":
    main()
