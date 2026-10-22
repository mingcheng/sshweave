"""Build and verify versioned release archives; never uploads or publishes."""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
)
DOCUMENTS = (
    "LICENSE",
    "README.md",
    "README_CN.md",
    "examples/README.md",
    "examples/README_CN.md",
    "examples/sshweave.toml",
)
COMPLETIONS = {"bash": "sshweave.bash", "fish": "sshweave.fish", "zsh": "_sshweave"}
NUMBER = r"(?:0|[1-9][0-9]*)"
PRERELEASE = r"(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
VERSION = re.compile(
    rf"{NUMBER}\.{NUMBER}\.{NUMBER}"
    rf"(?:-{PRERELEASE}(?:\.{PRERELEASE})*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def metadata(root, tag=None):
    with (root / "Cargo.toml").open("rb") as source:
        package = tomllib.load(source)["package"]
    version = package.get("version")
    if (
        package.get("name") != "sshweave"
        or not isinstance(version, str)
        or not VERSION.fullmatch(version)
    ):
        raise ValueError("expected an sshweave package with an explicit version")
    if tag is not None and tag != f"v{version}":
        raise ValueError(f"tag {tag!r} does not match Cargo.toml version v{version}")
    return {"version": version, "prerelease": "-" in version.split("+", 1)[0]}


def command_output(binary, args, home):
    environment = dict(os.environ, HOME=str(home))
    environment.pop("SSHWEAVE_CONFIG", None)
    result = subprocess.run(
        [str(binary), *args],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        env=environment,
        timeout=15,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"{binary.name} {' '.join(args)} exited {result.returncode}: "
            f"{result.stderr.strip()}"
        )
    return result.stdout


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def package(root, binary, target, output, epoch):
    if target not in TARGETS:
        raise ValueError(f"unsupported release target {target!r}")
    if not 0 <= epoch <= 0xFFFFFFFF:
        raise ValueError("archive epoch must fit an unsigned 32-bit timestamp")
    version = metadata(root)["version"]
    name = f"sshweave-{version}-{target}"
    archive = output / f"{name}.tar.gz"
    checksum = output / f"{archive.name}.sha256"
    if archive.exists() or checksum.exists():
        raise FileExistsError(f"release artifacts already exist for {name}")
    binary = binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="sshweave-release-") as temporary:
        home = Path(temporary) / "home"
        home.mkdir()
        actual_version = command_output(binary, ["--version"], home).strip()
        if actual_version != f"sshweave {version}":
            raise ValueError(f"binary version mismatch: {actual_version!r}")
        staging = Path(temporary) / name
        staging.mkdir()
        shutil.copyfile(binary, staging / "sshweave")
        for document in DOCUMENTS:
            destination = staging / document
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(root / document, destination)
        completions = staging / "completions"
        completions.mkdir()
        for shell, filename in COMPLETIONS.items():
            content = command_output(binary, ["completions", shell], home)
            if not content.strip():
                raise ValueError(f"empty {shell} completion output")
            (completions / filename).write_text(
                content, encoding="utf-8"
            )

        def normalize(member):
            member.uid = member.gid = 0
            member.uname = member.gname = ""
            member.mtime = epoch
            member.mode = 0o755 if member.isdir() or member.name == f"{name}/sshweave" else 0o644
            return member

        output.mkdir(parents=True, exist_ok=True)
        with archive.open("xb") as destination:
            with gzip.GzipFile(filename="", fileobj=destination, mode="wb", mtime=epoch) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as bundle:
                    bundle.add(staging, arcname=name, filter=normalize)
    checksum.write_text(f"{digest(archive)}  {archive.name}\n", encoding="utf-8")
    return archive


def verify(root, directory):
    version = metadata(root)["version"]
    names = [f"sshweave-{version}-{target}.tar.gz" for target in TARGETS]
    expected = set(names) | {f"{name}.sha256" for name in names}
    actual = {path.name for path in directory.iterdir()}
    if missing := expected - actual:
        raise ValueError(f"missing release artifacts: {', '.join(sorted(missing))}")
    if extra := actual - expected - {"SHA256SUMS"}:
        raise ValueError(f"unexpected release artifacts: {', '.join(sorted(extra))}")
    lines = []
    for name in names:
        line = f"{digest(directory / name)}  {name}\n"
        if (directory / f"{name}.sha256").read_text(encoding="utf-8") != line:
            raise ValueError(f"checksum mismatch for {name}")
        lines.append(line)
    sums = directory / "SHA256SUMS"
    content = "".join(lines)
    if sums.exists() and sums.read_text(encoding="utf-8") != content:
        raise ValueError("existing SHA256SUMS does not match the verified artifacts")
    sums.write_text(content, encoding="utf-8")
    return sums


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    information = commands.add_parser("metadata", help="validate package version and optional tag")
    information.add_argument("--tag")
    information.add_argument("--github-output", type=Path)
    archive = commands.add_parser("package", help="package a natively runnable release binary")
    archive.add_argument("--target", choices=TARGETS, required=True)
    archive.add_argument("--binary", type=Path, required=True)
    archive.add_argument("--output", type=Path, default=Path("dist"))
    archive.add_argument("--epoch", type=int, default=0)
    verification = commands.add_parser("verify", help="require and checksum all four platform archives")
    verification.add_argument("--directory", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if args.command == "metadata":
        value = metadata(ROOT, args.tag)
        if args.github_output is not None:
            with args.github_output.open("a", encoding="utf-8") as output:
                output.write(f"version={value['version']}\n")
                output.write(f"prerelease={str(value['prerelease']).lower()}\n")
        print(json.dumps(value))
    elif args.command == "package":
        print(package(ROOT, args.binary, args.target, args.output, args.epoch))
    else:
        print(verify(ROOT, args.directory))


if __name__ == "__main__":
    try:
        main()
    except (OSError, KeyError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"release: {error}", file=sys.stderr)
        sys.exit(1)
