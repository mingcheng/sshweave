"""Measure end-to-end configuration checking with isolated synthetic inputs."""

import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time


def positive_integer(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def run(binary, source, environment):
    result = subprocess.run(
        [str(binary), "-c", str(source), "config", "check"],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        env=environment,
        timeout=15,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"config check exited {result.returncode}: {result.stderr.strip()}"
        )


def benchmark(binary, iterations):
    results = []
    with tempfile.TemporaryDirectory(prefix="sshweave-benchmark-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        environment = dict(os.environ, HOME=str(home), USER="benchmark")
        for name in (
            "SSHWEAVE_CONFIG",
            "SSHWEAVE_DEBUG",
            "SSHWEAVE_ROUTE_STACK",
            "SSHWEAVE_ROUTE_PROXY",
            "RUST_LOG",
        ):
            environment.pop(name, None)
        for kind, counts in (("literal", (200, 600, 1800)), ("regex", (20, 60, 180))):
            for count in counts:
                source = root / f"{kind}-{count}.toml"
                entries = []
                for index in range(count):
                    name = (
                        f"node{index:05}"
                        if kind == "literal"
                        else f"regex:^node{index:05}-[0-9]+$"
                    )
                    entries.append(
                        f"[hosts.{json.dumps(name)}]\nhostname='127.0.0.1'\n"
                    )
                source.write_text(
                    "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\n"
                    "[defaults]\nuser='benchmark'\n" + "\n".join(entries),
                    encoding="utf-8",
                )
                run(binary, source, environment)
                samples = []
                for _ in range(iterations):
                    started = time.perf_counter()
                    run(binary, source, environment)
                    samples.append(round((time.perf_counter() - started) * 1000, 3))
                results.append(
                    {
                        "kind": kind,
                        "entries": count,
                        "median_ms": statistics.median(samples),
                        "samples_ms": samples,
                    }
                )
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "target/release/sshweave",
    )
    parser.add_argument("--iterations", type=positive_integer, default=3)
    parser.add_argument(
        "--max-regex-ms",
        type=float,
        help="fail if the median for 180 regex rules exceeds this budget",
    )
    args = parser.parse_args()
    if args.max_regex_ms is not None and not 0 < args.max_regex_ms < float("inf"):
        parser.error("--max-regex-ms must be finite and positive")
    results = benchmark(args.binary.resolve(strict=True), args.iterations)
    print(json.dumps({"warmups": 1, "results": results}, indent=2))
    if args.max_regex_ms is not None and results[-1]["median_ms"] > args.max_regex_ms:
        raise RuntimeError(
            f"180-rule median {results[-1]['median_ms']} ms exceeds "
            f"the {args.max_regex_ms} ms budget"
        )


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"benchmark: {error}", file=sys.stderr)
        sys.exit(1)
