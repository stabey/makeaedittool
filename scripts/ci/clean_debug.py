"""Reclaim only this workspace's two debug trees after successful workspace tests."""

import argparse
import shutil

from ci_common import ROOT, TARGETS


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    args = parser.parse_args()
    target = ROOT / "target"
    for path in (target, target / args.target, target / args.target / "debug", target / "debug"):
        if path.is_symlink():
            raise RuntimeError(f"Refusing to clean a symlinked build directory: {path}")
    for path in (target / args.target / "debug", target / "debug"):
        if path.exists():
            print(f"Removing {path}", flush=True)
            shutil.rmtree(path)
    print(f"free_disk_bytes_after_debug_cleanup={shutil.disk_usage(ROOT).free}", flush=True)


if __name__ == "__main__":
    main()
