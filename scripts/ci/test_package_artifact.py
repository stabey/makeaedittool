"""Packaging behavior tests: machine headers, filtered attribution, and notice edges."""

from pathlib import Path
import os
import struct
import tempfile
import unittest
import zipfile

from ci_common import TARGETS, save_support
from package_artifact import add_file, crate_notices, machine_header, resolved_dependencies, write_archive


class PackagingTests(unittest.TestCase):
    def test_save_support_matches_supported_platforms(self):
        for target, expected in (
            ("aarch64-apple-darwin", "SUPPORTED (macOS)"),
            ("x86_64-apple-darwin", "SUPPORTED (macOS)"),
            ("x86_64-pc-windows-msvc", "SUPPORTED (Windows)"),
            ("aarch64-pc-windows-msvc", "SUPPORTED (Windows)"),
            ("x86_64-unknown-linux-gnu", "SUPPORTED (Linux)"),
            ("aarch64-unknown-linux-gnu", "SUPPORTED (Linux)"),
            ("x86_64-unknown-freebsd", "UNSUPPORTED"),
            ("wasm32-unknown-unknown", "UNSUPPORTED"),
        ):
            with self.subTest(target=target):
                self.assertEqual(save_support(target), expected)

    def test_machine_headers_match_the_requested_architecture(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "binary"
            for target in TARGETS:
                with self.subTest(target=target):
                    header = bytearray(64)
                    if "apple" in target:
                        cpu = 0x0100000C if target.startswith("aarch64") else 0x01000007
                        struct.pack_into("<IIII", header, 0, 0xFEEDFACF, cpu, 0, 2)
                    elif "windows" in target:
                        header[:2] = b"MZ"
                        struct.pack_into("<I", header, 60, 64)
                        machine = 0xAA64 if target.startswith("aarch64") else 0x8664
                        header.extend(b"PE\0\0" + struct.pack("<H", machine))
                    else:
                        header[:6] = b"\x7fELF\x02\x01"
                        struct.pack_into("<H", header, 18, 62)
                    path.write_bytes(header)
                    self.assertTrue(machine_header(path, target))
                    for other in TARGETS:
                        if other != target:
                            with self.assertRaises(RuntimeError):
                                machine_header(path, other)

    def test_truncated_and_invalid_pe_offsets_fail(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "binary"
            path.write_bytes(b"MZ")
            for target in TARGETS:
                with self.assertRaises(RuntimeError):
                    machine_header(path, target)
            for offset in (0, 63, 64, 4096):
                header = bytearray(64)
                header[:2] = b"MZ"
                struct.pack_into("<I", header, 60, offset)
                path.write_bytes(header)
                with self.assertRaises(RuntimeError):
                    machine_header(path, "aarch64-pc-windows-msvc")

    def test_platform_inventory_walks_resolve_not_the_packages_array(self):
        packages = [
            {"id": name, "name": name, "version": "1.0.0"}
            for name in ("workspace", "native", "native-build", "other-platform")
        ]
        metadata = {
            "packages": packages,
            "workspace_members": ["workspace"],
            "resolve": {"nodes": [
                {"id": "workspace", "dependencies": ["native"]},
                {"id": "native", "dependencies": ["native-build"]},
                {"id": "native-build", "dependencies": []},
                {"id": "other-platform", "dependencies": []},
            ]},
        }
        self.assertEqual([p["id"] for p in resolved_dependencies(metadata)], ["native", "native-build"])

    def test_null_license_uses_declared_and_case_insensitive_notices(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            (root / "LiCeNsEs").mkdir()
            for name in ("LICENSE", "notice.txt", "LiCeNsEs/BoringSSL.txt", "terms.txt"):
                (root / name).write_text("License text\n", encoding="utf-8")
            expression, files = crate_notices({
                "id": "crate", "manifest_path": str(root / "Cargo.toml"),
                "license": None, "license_file": "terms.txt",
            })
            self.assertTrue(expression.startswith("SEE LICENSE FILE ("))
            self.assertEqual(set(files), {"LICENSE", "notice.txt", "LiCeNsEs/BoringSSL.txt", "terms.txt"})

    def test_missing_declared_or_null_unattributed_license_fails(self):
        with tempfile.TemporaryDirectory() as scratch:
            package = {"id": "crate", "manifest_path": str(Path(scratch) / "Cargo.toml"), "license": None}
            with self.assertRaises(RuntimeError):
                crate_notices(package)
            with self.assertRaises(RuntimeError):
                crate_notices({**package, "license": "MIT", "license_file": "missing-LICENSE"})

    def test_zip_entries_cannot_escape_or_overwrite(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "LICENSE"
            path.write_text("License text\n", encoding="utf-8")
            files = {}
            add_file(files, "licenses/LICENSE", path)
            for name in ("licenses/LICENSE", "../LICENSE", "/LICENSE", "licenses\\LICENSE"):
                with self.assertRaises(RuntimeError):
                    add_file(files, name, path)

    def test_pre_dos_license_timestamp_preserves_bytes_and_mode(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            notice = root / "LICENSE"
            content = b"Copyright notice from a published crate\n"
            notice.write_bytes(content)
            notice.chmod(0o640)
            os.utime(notice, (1, 1))
            archive = root / "artifact.zip"
            write_archive(archive, {"licenses/LICENSE": notice}, {"BUILD_IDENTITY.txt": "test"})
            with zipfile.ZipFile(archive) as bundle:
                self.assertEqual(bundle.read("licenses/LICENSE"), content)
                entry = bundle.getinfo("licenses/LICENSE")
                self.assertEqual(entry.date_time, (1980, 1, 1, 0, 0, 0))
                if os.name != "nt":
                    self.assertEqual((entry.external_attr >> 16) & 0o777, 0o640)


if __name__ == "__main__":
    unittest.main()
