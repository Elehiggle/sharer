import hashlib
import json
import re
import sys
import zipfile
from pathlib import Path


def version_parts(version: str) -> tuple[int, int, int]:
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version):
        raise ValueError(f"Expected a stable version, got {version!r}")
    return tuple(map(int, version.split(".")))


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit("Usage: update-scoop-manifest.py MANIFEST ARCHIVE TAG")

    manifest_path = Path(sys.argv[1])
    archive_path = Path(sys.argv[2])
    tag = sys.argv[3]
    if not tag.startswith("v"):
        raise ValueError(f"Expected a v-prefixed release tag, got {tag!r}")
    version = tag[1:]
    target_version = version_parts(version)

    with zipfile.ZipFile(archive_path) as archive:
        if "sharer-windows-x86_64/ShareR.exe" not in archive.namelist():
            raise ValueError("Release ZIP does not contain sharer-windows-x86_64/ShareR.exe")
        if damaged_entry := archive.testzip():
            raise ValueError(f"Release ZIP contains a damaged entry: {damaged_entry}")

    digest = hashlib.sha256()
    with archive_path.open("rb") as archive:
        for chunk in iter(lambda: archive.read(1024 * 1024), b""):
            digest.update(chunk)
    archive_hash = digest.hexdigest()

    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    current_version = version_parts(manifest["version"])
    if current_version > target_version:
        print(f"Scoop manifest is already newer than {tag}; leaving it unchanged")
        return

    package = manifest["architecture"]["64bit"]
    url = f"https://github.com/legacy3/sharer/releases/download/{tag}/sharer-windows-x86_64.zip"
    if current_version == target_version:
        if package["url"] != url or package["hash"].lower() != archive_hash:
            raise ValueError(f"Published {tag} differs from the existing Scoop manifest")
        print(f"Scoop manifest already matches {tag}")
        return

    manifest["version"] = version
    package["url"] = url
    package["hash"] = archive_hash
    manifest_path.write_text(json.dumps(manifest, indent=4) + "\n", encoding="utf-8")
    print(f"Updated Scoop manifest to {tag} ({archive_hash})")


if __name__ == "__main__":
    main()
