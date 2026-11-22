"""Measure the uncompressed files included in an installation."""

import argparse
from pathlib import Path


LIMIT_BYTES = 1024 * 1024


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("files", type=Path, nargs="+")
    arguments = parser.parse_args()
    total = 0
    for path in arguments.files:
        if not path.is_file():
            parser.error(f"Missing installation file: {path}")
        size = path.stat().st_size
        total += size
        print(f"{path}: {size} bytes")
    print(f"Installation: {total}/{LIMIT_BYTES} bytes")
    raise SystemExit(total > LIMIT_BYTES)


if __name__ == "__main__":
    main()
