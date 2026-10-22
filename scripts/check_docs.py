"""Check local file targets in tracked Markdown links, without network access."""

from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
LINK = re.compile(r"\[[^\]\n]*\]\((<[^>\n]+>|[^\s)\n]+)(?:\s+\"[^\"]*\")?\)")


def broken_links(document):
    failures = []
    for number, line in enumerate(document.read_text(encoding="utf-8").splitlines(), 1):
        for match in LINK.finditer(line):
            target = match.group(1).strip("<>")
            parsed = urlsplit(target)
            if parsed.scheme or parsed.netloc or not parsed.path:
                continue
            destination = document.parent / unquote(parsed.path)
            if not destination.exists():
                failures.append(f"{document}:{number}: missing link target {target}")
    return failures


def main():
    paths = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.md"],
        cwd=ROOT,
        capture_output=True,
        check=True,
    ).stdout.decode("utf-8").split("\0")
    failures = [
        failure
        for path in paths
        if path
        for failure in broken_links(ROOT / path)
    ]
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("All tracked Markdown file links resolve (remote URLs and anchors are not checked).")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"check-docs: {error}", file=sys.stderr)
        sys.exit(1)
