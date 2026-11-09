"""Writes a crafted .tar.gz for install_test.sh.

Usage: make_archive.py OUT ENTRY...
  file:NAME:PATH   a regular file NAME with the bytes and mode of PATH
  link:NAME:TARGET a symbolic link NAME pointing at TARGET
"""
import os
import sys
import tarfile


def main() -> None:
    out = sys.argv[1]
    with tarfile.open(out, "w:gz") as archive:
        for entry in sys.argv[2:]:
            kind, name, value = entry.split(":", 2)
            if kind == "file":
                info = archive.gettarinfo(value, arcname=name)
                with open(value, "rb") as handle:
                    archive.addfile(info, handle)
            elif kind == "link":
                info = tarfile.TarInfo(name)
                info.type = tarfile.SYMTYPE
                info.linkname = value
                info.mode = 0o755
                archive.addfile(info)
            else:
                raise SystemExit(f"unknown entry kind: {kind}")
    os.chmod(out, 0o644)


if __name__ == "__main__":
    main()
