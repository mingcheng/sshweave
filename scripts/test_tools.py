"""Offline regression tests for release, benchmark, and documentation tooling."""

import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError

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
        version = release.metadata(self.root)["version"]
        for target in release.TARGETS:
            path = output / f"sshweave-{version}-{target}.tar.gz"
            path.write_bytes(target.encode())
            checksum = hashlib.sha256(path.read_bytes()).hexdigest()
            (output / f"{path.name}.sha256").write_text(
                f"{checksum}  {path.name}\n", encoding="utf-8"
            )
        return output

    def release_record(self, output, draft=False):
        information = release.metadata(self.root)
        tag = f"v{information['version']}"
        return {
            "tag_name": tag,
            "draft": draft,
            "prerelease": information["prerelease"],
            "html_url": f"https://github.com/example/sshweave/releases/tag/{tag}",
            "assets": [
                {
                    "name": path.name,
                    "state": "uploaded",
                    "digest": f"sha256:{hashlib.sha256(path.read_bytes()).hexdigest()}",
                }
                for path in sorted(output.iterdir())
            ],
        }

    def crate_file(self):
        version = release.metadata(self.root)["version"]
        archive = self.root / "package" / f"sshweave-{version}.crate"
        archive.parent.mkdir(exist_ok=True)
        archive.write_bytes(b"packaged crate")
        return archive

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
            self.assertTrue(archive.getmember(f"{prefix}/sshweave.webp").isfile())
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

    def test_publication_uploads_all_assets_before_publishing_the_draft(self):
        output = self.collection()
        url = "https://github.com/example/sshweave/releases/tag/v1.2.3"
        with patch.object(release, "github", side_effect=["[[]]", "draft URL", "", "", url]) as github:
            self.assertEqual(release.publish(self.root, output, "v1.2.3", "example/sshweave"), url)
        calls = [call.args for call in github.call_args_list]
        self.assertEqual(calls[0], ("api", "--paginate", "--slurp", "repos/example/sshweave/releases?per_page=100"))
        self.assertEqual(calls[1], (
            "release", "create", "v1.2.3", "--repo", "example/sshweave",
            "--verify-tag", "--generate-notes", "--title", "v1.2.3", "--draft", "--prerelease=false",
        ))
        self.assertEqual(calls[2], (
            "release", "upload", "v1.2.3", "--repo", "example/sshweave", "--clobber",
            *(str(path.resolve()) for path in sorted(output.iterdir())),
        ))
        self.assertEqual(len(list(output.iterdir())), 9)
        self.assertEqual(calls[3], (
            "release", "edit", "v1.2.3", "--repo", "example/sshweave",
            "--verify-tag", "--draft=false", "--prerelease=false",
        ))
        self.assertEqual(calls[4], (
            "release", "view", "v1.2.3", "--repo", "example/sshweave", "--json", "url", "--jq", ".url",
        ))

    def test_prerelease_tag_is_not_published_as_a_stable_release(self):
        (self.root / "Cargo.toml").write_text(
            '[package]\nname="sshweave"\nversion="1.2.3-rc.1"\n', encoding="utf-8"
        )
        output = self.collection()
        with patch.object(release, "github", side_effect=["[[]]", "draft URL", "", "", "release URL"]) as github:
            release.publish(self.root, output, "v1.2.3-rc.1", "example/sshweave")
        self.assertIn("--prerelease=true", github.call_args_list[1].args)
        self.assertIn("--prerelease=true", github.call_args_list[3].args)

    def test_partial_draft_is_resumed_without_creating_another_release(self):
        output = self.collection()
        draft = self.release_record(output, draft=True)
        draft["assets"] = draft["assets"][:2]
        draft["assets"][0]["state"] = "starter"
        pages = [[{"tag_name": "v0.0.1"}], [draft]]
        with patch.object(release, "github", side_effect=[json.dumps(pages), "", "", draft["html_url"]]) as github:
            release.publish(self.root, output, "v1.2.3", "example/sshweave")
        self.assertEqual([call.args[:2] for call in github.call_args_list[1:]], [
            ("release", "upload"), ("release", "edit"), ("release", "view"),
        ])
        self.assertIn("--clobber", github.call_args_list[1].args)

    def test_upload_failure_leaves_the_release_unpublished(self):
        output = self.collection()
        error = subprocess.CalledProcessError(1, ["gh", "release", "upload"])
        with patch.object(release, "github", side_effect=["[[]]", "draft URL", error]) as github:
            with self.assertRaises(subprocess.CalledProcessError):
                release.publish(self.root, output, "v1.2.3", "example/sshweave")
        self.assertEqual(github.call_count, 3)
        self.assertEqual(github.call_args.args[:2], ("release", "upload"))

    def test_matching_published_release_is_verified_without_modification(self):
        output = self.collection()
        release.verify(self.root, output)
        published = self.release_record(output)
        with patch.object(release, "github", return_value=json.dumps([[published]])) as github:
            self.assertEqual(
                release.publish(self.root, output, "v1.2.3", "example/sshweave"),
                published["html_url"],
            )
        self.assertEqual(github.call_count, 1)

    def test_different_or_unverified_published_assets_are_never_overwritten(self):
        output = self.collection()
        release.verify(self.root, output)
        for difference in ("digest", "missing", "state", "unknown_digest", "prerelease"):
            with self.subTest(difference=difference):
                published = self.release_record(output)
                if difference == "digest":
                    published["assets"][0]["digest"] = "sha256:wrong"
                elif difference == "missing":
                    published["assets"].pop()
                elif difference == "state":
                    published["assets"][0]["state"] = "starter"
                elif difference == "unknown_digest":
                    published["assets"][0].pop("digest")
                else:
                    published["prerelease"] = True
                with patch.object(release, "github", return_value=json.dumps([[published]])) as github:
                    with self.assertRaisesRegex(ValueError, "published release .* refusing to overwrite"):
                        release.publish(self.root, output, "v1.2.3", "example/sshweave")
                self.assertEqual(github.call_count, 1)

    def test_unexpected_draft_assets_and_ambiguous_releases_are_rejected(self):
        output = self.collection()
        draft = self.release_record(output, draft=True)
        with patch.object(release, "github", return_value=json.dumps([[draft, draft]])) as github:
            with self.assertRaisesRegex(ValueError, "multiple GitHub releases"):
                release.publish(self.root, output, "v1.2.3", "example/sshweave")
        self.assertEqual(github.call_count, 1)
        draft["assets"].append({"name": "unexpected.zip"})
        with patch.object(release, "github", return_value=json.dumps([[draft]])) as github:
            with self.assertRaisesRegex(ValueError, "unexpected draft release assets"):
                release.publish(self.root, output, "v1.2.3", "example/sshweave")
        self.assertEqual(github.call_count, 1)

    def test_invalid_local_release_is_rejected_before_contacting_github(self):
        output = self.collection()
        with patch.object(release, "github") as github:
            with self.assertRaisesRegex(ValueError, "does not match"):
                release.publish(self.root, output, "v1.2.4", "example/sshweave")
            next(output.glob("*.tar.gz")).write_bytes(b"corrupted")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                release.publish(self.root, output, "v1.2.3", "example/sshweave")
        github.assert_not_called()

    def test_github_errors_are_not_treated_as_an_absent_release(self):
        output = self.collection()
        error = subprocess.CalledProcessError(1, ["gh", "api"])
        with patch.object(release, "github", side_effect=error) as github:
            with self.assertRaises(subprocess.CalledProcessError):
                release.publish(self.root, output, "v1.2.3", "example/sshweave")
        self.assertEqual(github.call_count, 1)

    def test_github_commands_are_noninteractive_and_propagate_failures(self):
        with patch.object(release.subprocess, "run") as run:
            run.return_value.stdout = "response"
            self.assertEqual(release.github("api", "repos/example/sshweave/releases"), "response")
            run.assert_called_once_with(
                ["gh", "api", "repos/example/sshweave/releases"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                text=True,
                timeout=300,
                check=True,
            )
            run.side_effect = subprocess.CalledProcessError(1, ["gh", "api"])
            with self.assertRaises(subprocess.CalledProcessError):
                release.github("api", "repos/example/sshweave/releases")

    def test_crate_status_verifies_identical_versions_including_prereleases(self):
        for version in ("1.2.3", "1.2.3-rc.1"):
            with self.subTest(version=version):
                (self.root / "Cargo.toml").write_text(
                    f'[package]\nname="sshweave"\nversion="{version}"\n', encoding="utf-8"
                )
                archive = self.crate_file()
                entry = {
                    "name": "sshweave", "vers": version, "yanked": False,
                    "cksum": hashlib.sha256(archive.read_bytes()).hexdigest(),
                }
                response = io.BytesIO(json.dumps(entry).encode())
                with patch.object(release, "urlopen", return_value=response) as request:
                    self.assertEqual(
                        release.crate_status(self.root, archive.parent, f"v{version}"),
                        {"version": version, "published": True},
                    )
                self.assertEqual(request.call_args.args[0].full_url, "https://index.crates.io/ss/hw/sshweave")
                self.assertEqual(request.call_args.kwargs, {"timeout": 30})

    def test_crate_status_allows_only_a_genuinely_absent_version(self):
        archive = self.crate_file()
        response = io.BytesIO(b'{"name":"sshweave","vers":"1.2.2"}\n')
        with patch.object(release, "urlopen", return_value=response):
            self.assertEqual(
                release.crate_status(self.root, archive.parent, "v1.2.3"),
                {"version": "1.2.3", "published": False},
            )
        error = HTTPError("https://index.crates.io/ss/hw/sshweave", 404, "Not Found", {}, None)
        with patch.object(release, "urlopen", side_effect=error):
            self.assertFalse(release.crate_status(self.root, archive.parent, "v1.2.3")["published"])
        self.assertTrue(error.closed)

    def test_crate_status_rejects_yanked_changed_or_ambiguous_versions(self):
        archive = self.crate_file()
        for failure, message in (
            ("changed", "checksum mismatch"),
            ("yanked", "is yanked"),
            ("duplicate", "duplicate crates.io index"),
        ):
            with self.subTest(failure=failure):
                entry = {
                    "name": "sshweave", "vers": "1.2.3", "yanked": failure == "yanked",
                    "cksum": "wrong" if failure == "changed" else hashlib.sha256(archive.read_bytes()).hexdigest(),
                }
                content = json.dumps(entry) + "\n"
                if failure == "duplicate":
                    content *= 2
                with patch.object(release, "urlopen", return_value=io.BytesIO(content.encode())):
                    with self.assertRaisesRegex(ValueError, message):
                        release.crate_status(self.root, archive.parent, "v1.2.3")

    def test_crate_status_does_not_hide_registry_errors(self):
        archive = self.crate_file()
        for code in (403, 429, 500):
            with self.subTest(code=code):
                error = HTTPError("https://index.crates.io/ss/hw/sshweave", code, "Registry error", {}, None)
                with patch.object(release, "urlopen", side_effect=error):
                    with self.assertRaises(HTTPError):
                        release.crate_status(self.root, archive.parent, "v1.2.3")
                self.assertTrue(error.closed)
        with patch.object(release, "urlopen", side_effect=URLError("connection failed")):
            with self.assertRaises(URLError):
                release.crate_status(self.root, archive.parent, "v1.2.3")
        for content in (b"", b"invalid JSON", b'{"name":"another-crate","vers":"1.2.3"}'):
            with self.subTest(content=content):
                with patch.object(release, "urlopen", return_value=io.BytesIO(content)):
                    with self.assertRaises(ValueError):
                        release.crate_status(self.root, archive.parent, "v1.2.3")

    def test_crate_status_validates_local_inputs_before_registry_access(self):
        with patch.object(release, "urlopen") as request:
            with self.assertRaisesRegex(ValueError, "does not match"):
                release.crate_status(self.root, self.root / "package", "v1.2.4")
            with self.assertRaises(FileNotFoundError):
                release.crate_status(self.root, self.root / "package", "v1.2.3")
        request.assert_not_called()

    def test_crate_status_cli_writes_boolean_github_outputs(self):
        archive = self.crate_file()
        output = self.root / "github-output"
        arguments = [
            "release.py", "crate-status", "--tag", "v1.2.3",
            "--directory", str(archive.parent), "--github-output", str(output),
        ]
        for published in (False, True):
            with self.subTest(published=published):
                output.write_text("existing=value\n", encoding="utf-8")
                value = {"version": "1.2.3", "published": published}
                with (
                    patch.object(release, "ROOT", self.root),
                    patch.object(release.sys, "argv", arguments),
                    patch.object(release, "crate_status", return_value=value) as status,
                    patch.object(release.sys, "stdout", new_callable=io.StringIO) as stdout,
                ):
                    release.main()
                status.assert_called_once_with(self.root, archive.parent, "v1.2.3")
                self.assertEqual(json.loads(stdout.getvalue()), value)
                self.assertEqual(
                    output.read_text(encoding="utf-8"),
                    f"existing=value\npublished={str(published).lower()}\n",
                )


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
