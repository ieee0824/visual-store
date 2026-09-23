#!/usr/bin/env python3
"""Build, verify, and package a tagged vstore release."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)


def run(*args, env=None):
    return subprocess.run(args, cwd=ROOT, env=env, check=True, text=True, capture_output=True).stdout


def version_for(tag):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    expected = f"v{version}"
    if tag and tag != expected:
        raise SystemExit(f"Release tag {tag!r} does not match Cargo.toml version {expected!r}")
    return expected


def check_host(target):
    system = {"Linux": "unknown-linux-gnu", "Darwin": "apple-darwin"}.get(platform.system())
    arch = {"AMD64": "x86_64", "x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(platform.machine())
    if f"{arch}-{system}" != target:
        raise SystemExit(f"Expected native build for {target}, got {arch}-{system}")


def archive_name(tag, target):
    return f"vstore-{tag}-{target}.tar.gz"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_checksums(tag):
    version_for(tag)
    expected = [archive_name(tag, target) for target in TARGETS]
    if {path.name for path in DIST.glob("*.tar.gz")} != set(expected):
        raise SystemExit("Expected exactly four release archives before writing checksums")
    (DIST / "SHA256SUMS").write_text(
        "".join(f"{digest(DIST / name)}  {name}\n" for name in sorted(expected))
    )


def verify_dist(tag):
    version_for(tag)
    expected = {archive_name(tag, target) for target in TARGETS}
    actual = {path.name for path in DIST.glob("*.tar.gz")}
    if actual != expected:
        raise SystemExit(f"Wrong release assets: expected {sorted(expected)}, got {sorted(actual)}")
    checksums = (DIST / "SHA256SUMS").read_text().splitlines()
    expected_lines = [f"{digest(DIST / name)}  {name}" for name in sorted(expected)]
    if checksums != expected_lines:
        raise SystemExit("SHA256SUMS does not match release assets")
    for name in expected:
        with tarfile.open(DIST / name, "r:gz") as archive:
            files = set(archive.getnames())
            prefix = name.removesuffix(".tar.gz")
            required = {
                f"{prefix}/vstore",
                f"{prefix}/LICENSE",
                f"{prefix}/LIBVPX_LICENSE",
                f"{prefix}/THIRD_PARTY_NOTICES.txt",
            }
            if not required <= files:
                raise SystemExit(f"Missing packaged files in {name}: {required - files}")
    print("Verified four release archives and SHA256SUMS")


def add_notices(package, target):
    metadata = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target))
    notices = ["Rust dependency notices (Cargo.lock versions)\n"]
    licenses = package / "licenses"
    for crate in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        if crate["name"] == "visual-store":
            continue
        name, version = crate["name"], crate["version"]
        notices.append(f"\n{name} {version}: {crate.get('license') or 'SEE LICENSE FILE'}\n")
        notices.append(f"Source: {crate.get('repository') or crate.get('homepage') or crate.get('source')}\n")
        source = Path(crate["manifest_path"]).parent
        files = [path for path in source.iterdir() if path.is_file() and re.match(r"(?i)^(license|licence|copying|notice)([._-].*)?$", path.name)]
        for path in files:
            destination = licenses / f"{name}-{version}" / path.name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, destination)
            notices.append(f"License file: licenses/{name}-{version}/{path.name}\n")
    (package / "THIRD_PARTY_NOTICES.txt").write_text("".join(notices))


def build(target, tag):
    if target not in TARGETS:
        raise SystemExit(f"Unsupported target: {target}")
    check_host(target)
    tag = version_for(tag)
    env = os.environ.copy()
    env["VPX_STATIC"] = "1"
    # pkg-config-rs intentionally avoids static linkage for libraries under
    # /usr on Linux. The binding's explicit-library path bypasses that rule.
    libdir = Path(run("pkg-config", "--variable=libdir", "vpx").strip())
    if not (libdir / "libvpx.a").is_file() and platform.system() == "Linux":
        libdir /= run("cc", "-print-multiarch").strip()
    env["VPX_LIB_DIR"] = str(libdir)
    env["VPX_INCLUDE_DIR"] = run("pkg-config", "--variable=includedir", "vpx").strip()
    env["VPX_VERSION"] = run("pkg-config", "--modversion", "vpx").strip()
    if not (libdir / "libvpx.a").is_file():
        raise SystemExit("Static libvpx archive is unavailable")
    subprocess.run(
        ["cargo", "build", "--locked", "--release", "--bin", "vstore", "--example", "vp9_roundtrip"],
        cwd=ROOT, env=env, check=True,
    )
    target_dir = Path(env.get("CARGO_TARGET_DIR", ROOT / "target"))
    binary = target_dir / "release/vstore"
    example = target_dir / "release/examples/vp9_roundtrip"
    if run(str(binary), "--version").strip() != f"vstore {tag[1:]}":
        raise SystemExit("Built binary version does not match release tag")
    run(str(example))
    dependencies = run("otool", "-L", str(binary)) if platform.system() == "Darwin" else run("ldd", str(binary))
    if re.search(r"libvpx|libavcodec|libavformat|libavutil|libswscale", dependencies):
        raise SystemExit(f"Unexpected dynamic codec dependency:\n{dependencies}")
    print(dependencies)

    DIST.mkdir(exist_ok=True)
    name = archive_name(tag, target)
    prefix = name.removesuffix(".tar.gz")
    with tempfile.TemporaryDirectory() as temporary:
        package = Path(temporary) / prefix
        package.mkdir()
        shutil.copy2(binary, package / "vstore")
        shutil.copy2(ROOT / "LICENSE", package / "LICENSE")
        shutil.copy2(ROOT / "licenses/libvpx/LICENSE", package / "LIBVPX_LICENSE")
        add_notices(package, target)
        with tarfile.open(DIST / name, "w:gz") as archive:
            archive.add(package, arcname=prefix)
    print(f"Created {DIST / name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", default="")
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--verify-dist", action="store_true")
    parser.add_argument("--write-checksums", action="store_true")
    args = parser.parse_args()
    if args.verify_dist or args.write_checksums:
        if not args.tag:
            parser.error("--verify-dist and --write-checksums require --tag")
        if args.write_checksums:
            write_checksums(args.tag)
        else:
            verify_dist(args.tag)
    elif args.target:
        build(args.target, args.tag)
    else:
        parser.error("--target or --verify-dist is required")


if __name__ == "__main__":
    main()
