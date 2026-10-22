"""Regression tests for local release, benchmark, and documentation tooling."""

import hashlib
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import benchmark
import check_docs
import release


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="sshweave-tool-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "Cargo.toml").write_text(
            '[package]\nname="sshweave"\nversion="1.2.3"\n', encoding="utf-8"
        )

    def collection(self):
        output = self.root / "dist"
        output.mkdir()
        for target in release.TARGETS:
            path = output / f"sshweave-1.2.3-{target}.tar.gz"
            path.write_bytes(target.encode())
            checksum = hashlib.sha256(path.read_bytes()).hexdigest()
            (output / f"{path.name}.sha256").write_text(
                f"{checksum}  {path.name}\n", encoding="utf-8"
            )
        return output

    def test_version_and_tag_validation(self):
        self.assertEqual(release.metadata(self.root, "v1.2.3"), {"version": "1.2.3", "prerelease": False})
        for tag in ("1.2.3", "v1.2.4", "main", "v1.2.3\n"):
            with self.assertRaises(ValueError):
                release.metadata(self.root, tag)
        (self.root / "Cargo.toml").write_text(
            '[package]\nname="sshweave"\nversion="1.2.3-rc.1+build"\n', encoding="utf-8"
        )
        self.assertTrue(release.metadata(self.root)["prerelease"])

    def test_invalid_versions_and_build_metadata(self):
        for version in ("01.2.3", "1.2.3-01", "1.2.3+", "1.2.3+build..meta", "../1.2.3"):
            (self.root / "Cargo.toml").write_text(
                f'[package]\nname="sshweave"\nversion="{version}"\n', encoding="utf-8"
            )
            with self.assertRaises(ValueError):
                release.metadata(self.root)
        (self.root / "Cargo.toml").write_text(
            '[package]\nname="sshweave"\nversion="1.2.3+build-with-hyphen"\n', encoding="utf-8"
        )
        self.assertFalse(release.metadata(self.root)["prerelease"])

    def test_archives_are_complete_and_reproducible(self):
        for document in release.DOCUMENTS:
            path = self.root / document
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(document, encoding="utf-8")
        binary = self.root / "binary"
        binary.write_bytes(b"test binary")

        def command_output(_binary, args, _home):
            return "sshweave 1.2.3\n" if args == ["--version"] else f"completion for {args[1]}\n"

        target = release.TARGETS[0]
        with patch.object(release, "command_output", side_effect=command_output):
            first = release.package(self.root, binary, target, self.root / "first", 123)
            second = release.package(self.root, binary, target, self.root / "second", 123)
            with self.assertRaises(FileExistsError):
                release.package(self.root, binary, target, self.root / "first", 123)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        prefix = f"sshweave-1.2.3-{target}"
        with tarfile.open(first) as archive:
            self.assertEqual(archive.getmember(f"{prefix}/sshweave").mode, 0o755)
            for document in release.DOCUMENTS:
                self.assertTrue(archive.getmember(f"{prefix}/{document}").isfile())
            for filename in release.COMPLETIONS.values():
                self.assertTrue(archive.getmember(f"{prefix}/completions/{filename}").isfile())
            self.assertTrue(all(member.uid == member.gid == 0 for member in archive.getmembers()))
            self.assertTrue(all(member.mtime == 123 for member in archive.getmembers()))
        self.assertEqual(
            first.with_name(first.name + ".sha256").read_text(encoding="utf-8"),
            f"{release.digest(first)}  {first.name}\n",
        )

    def test_wrong_binary_version_does_not_create_an_archive(self):
        binary = self.root / "binary"
        binary.write_bytes(b"test binary")
        with patch.object(release, "command_output", return_value="sshweave 0.0.0\n"):
            with self.assertRaisesRegex(ValueError, "binary version mismatch"):
                release.package(self.root, binary, release.TARGETS[0], self.root / "dist", 0)
        self.assertFalse((self.root / "dist").exists())

    def test_verification_requires_all_platforms_and_valid_checksums(self):
        output = self.collection()
        sums = release.verify(self.root, output)
        self.assertEqual(len(sums.read_text(encoding="utf-8").splitlines()), 4)
        self.assertEqual(release.verify(self.root, output), sums)
        archive = next(output.glob("*.tar.gz"))
        archive.write_bytes(b"corrupted")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            release.verify(self.root, output)

    def test_missing_or_extra_platform_assets_are_rejected(self):
        output = self.collection()
        extra = output / "unexpected.tar.gz"
        extra.write_bytes(b"extra")
        with self.assertRaisesRegex(ValueError, "unexpected release artifacts"):
            release.verify(self.root, output)
        extra.unlink()
        next(output.glob("*.sha256")).unlink()
        with self.assertRaisesRegex(ValueError, "missing release artifacts"):
            release.verify(self.root, output)

    def test_existing_inconsistent_manifest_is_rejected(self):
        output = self.collection()
        (output / "SHA256SUMS").write_text("wrong manifest\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "existing SHA256SUMS"):
            release.verify(self.root, output)


class DocumentationTests(unittest.TestCase):
    def test_local_targets_are_checked_without_fetching_remote_urls(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "with space.md").write_text("# Heading\n", encoding="utf-8")
            document = root / "README.md"
            document.write_text(
                '[local](with%20space.md#heading)\n'
                '[title](<with space.md> "Title")\n'
                '[web](https://example.invalid/missing.md)\n'
                '[mail](mailto:example@example.invalid)\n'
                '[anchor](#heading)\n'
                '[missing](missing.md)\n',
                encoding="utf-8",
            )
            failures = check_docs.broken_links(document)
            self.assertEqual(len(failures), 1)
            self.assertIn(":6: missing link target missing.md", failures[0])

    def test_benchmark_rejects_nonpositive_iterations(self):
        self.assertEqual(benchmark.positive_integer("3"), 3)
        for value in ("0", "-1"):
            with self.assertRaises(benchmark.argparse.ArgumentTypeError):
                benchmark.positive_integer(value)


if __name__ == "__main__":
    unittest.main()
