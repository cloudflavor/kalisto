import copy
import json
import os
from pathlib import Path
import struct
import tarfile
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import urllib.request

import release

SHA = "a" * 40
TAG = "v1.2.3"
SERVER = "https://api.github.com"
REPOSITORY = "example/kalisto"


def binary(target):
    data = bytearray(128)
    if "linux" in target:
        data[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", data, 18, 183 if target.startswith("aarch64") else 62)
        struct.pack_into("<Q", data, 32, 64)
        struct.pack_into("<HH", data, 54, 56, 1)
        struct.pack_into("<I", data, 64, 1)
    else:
        data[:4] = b"\xcf\xfa\xed\xfe"
        struct.pack_into("<I", data, 4, 0x0100000C)
    return bytes(data)


class FakeApi:
    def __init__(self, existing=None):
        self.release = copy.deepcopy(existing)
        self.assets = {}
        self.published = False
        self.moved = False
        self.tag_reads = 0
        self.fail_upload = False
        self.bad_readback = False
        self.drop_upload = False

    def tag_commit(self, tag):
        self.tag_reads += 1
        return "b" * 40 if self.moved and self.tag_reads > 1 else SHA

    def verify_source(self, tag, commit, expected):
        if self.tag_commit(tag) != commit or expected != {"type": "commit", "sha": SHA}:
            raise ValueError("source tag changed")

    def request(self, method, path, payload=None):
        if method == "GET" and path.startswith("/releases?per_page=100&page="):
            page = int(path.rsplit("=", 1)[1])
            history = getattr(self, "history", []) + ([self.release] if self.release else [])
            return history[(page - 1) * 100:page * 100]
        if method == "POST" and path == "/releases":
            assert payload["draft"]
            self.release = dict(payload, id=7, upload_url="https://uploads.github.com/assets{?name,label}")
            return self.release
        if method == "GET" and path == "/releases/7/assets?per_page=100":
            return [{"name": name, "id": index, "size": len(data), "url": f"{SERVER}/repos/{REPOSITORY}/releases/assets/{index}"}
                    for index, (name, data) in enumerate(self.assets.items())]
        if method == "PATCH":
            self.published = True
            self.release.update(payload)
            return self.release
        raise AssertionError((method, path))

    def upload(self, published, name, data):
        if self.fail_upload:
            raise release.ApiError(500)
        if not self.drop_upload:
            self.assets[name] = data

    def download(self, asset):
        return b"corrupt" if self.bad_readback else self.assets[asset["name"]]


class CandidateSource:
    def prepare(self, api, tag, commit):
        expected = {"type": "commit", "sha": SHA}
        api.verify_source(tag, commit, expected)
        return expected

    def verify(self, api, tag, commit, expected):
        api.verify_source(tag, commit, expected)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def candidates(self):
        for target in release.TARGETS:
            source = self.root / target
            source.write_bytes(binary(target))
            release.package(source, self.root / "dist", TAG, "1.2.3", SHA, target)
        return self.root / "dist"

    def test_tag_policy_and_prerelease_version(self):
        self.assertEqual(release.release_version("v1.2.3-rc.1+build.2"), "1.2.3-rc.1+build.2")
        for tag in ["latest", "v01.2.3", "v1.2", "v1.2.3-01", "v1.2.3;echo unsafe", "v1.2.3\n"]:
            with self.assertRaises(ValueError, msg=tag):
                release.release_version(tag)
        self.assertEqual(release.build_identity("refs/tags/v1.2.3", SHA, "5"), (TAG, "1.2.3"))
        self.assertEqual(release.build_identity("refs/heads/main", SHA, "5"), ("ci-aaaaaaaaaaaa", "0.0.0-ci.5+gaaaaaaaaaaaa"))

    def test_supported_platform_packages_and_inventory(self):
        dist = self.candidates()
        self.assertEqual(set(release.collect_artifacts(dist, TAG, SHA)), {
            f"kalisto-{TAG}-macos-arm64.tar.gz",
            f"kalisto-{TAG}-linux-amd64.tar.gz",
            f"kalisto-{TAG}-linux-arm64.tar.gz",
        })
        with tarfile.open(next(dist.glob("*.tar.gz"))) as archive:
            self.assertEqual(archive.getmember("kalisto").mode, 0o755)
            self.assertEqual(archive.getmember("kalisto").mtime, 0)

    def test_package_is_deterministic_and_never_overwrites(self):
        target = "aarch64-unknown-linux-musl"
        source = self.root / "binary"
        source.write_bytes(binary(target))
        a = release.package(source, self.root / "a", TAG, "1.2.3", SHA, target)
        b = release.package(source, self.root / "b", TAG, "1.2.3", SHA, target)
        self.assertEqual(a.read_bytes(), b.read_bytes())
        with self.assertRaises(FileExistsError):
            release.package(source, self.root / "a", TAG, "1.2.3", SHA, target)

    def test_wrong_architecture_dynamic_linux_or_incomplete_set_is_rejected(self):
        with self.assertRaises(ValueError):
            release.verify_binary(binary("x86_64-unknown-linux-musl"), "aarch64-unknown-linux-musl")
        macos = bytearray(binary("aarch64-apple-darwin"))
        struct.pack_into("<I", macos, 4, 0x01000007)
        with self.assertRaises(ValueError):
            release.verify_binary(macos, "aarch64-apple-darwin")
        data = bytearray(binary("aarch64-unknown-linux-musl"))
        struct.pack_into("<I", data, 64, 3)
        with self.assertRaises(ValueError):
            release.verify_binary(data, "aarch64-unknown-linux-musl")
        dist = self.candidates()
        next(dist.glob("*.tar.gz")).unlink()
        with self.assertRaises(ValueError):
            release.collect_artifacts(dist, TAG, SHA)

    def test_duplicate_artifacts_or_wrong_source_are_rejected(self):
        dist = self.candidates()
        with self.assertRaises(ValueError):
            release.collect_artifacts(dist, TAG, "b" * 40)
        nested = dist / "duplicate"
        nested.mkdir()
        source = next(dist.glob("*.tar.gz"))
        (nested / source.name).write_bytes(source.read_bytes())
        with self.assertRaises(ValueError):
            release.collect_artifacts(dist, TAG, SHA)

    def test_publish_reads_back_the_three_packages_and_checksum_inventory(self):
        api = FakeApi()
        result = release.publish(api, self.candidates(), TAG, SHA, CandidateSource())
        self.assertFalse(result["draft"])
        self.assertTrue(api.published)
        self.assertEqual(set(api.assets), {
            f"kalisto-{TAG}-macos-arm64.tar.gz",
            f"kalisto-{TAG}-linux-amd64.tar.gz",
            f"kalisto-{TAG}-linux-arm64.tar.gz",
            "SHA256SUMS",
        })
        self.assertEqual(len(api.assets["SHA256SUMS"].decode().splitlines()), 3)

    def test_upload_failure_corruption_or_moved_tag_leaves_draft(self):
        dist = self.candidates()
        for flag in ["fail_upload", "bad_readback", "moved", "drop_upload"]:
            api = FakeApi()
            setattr(api, flag, True)
            with self.assertRaises((ValueError, release.ApiError), msg=flag):
                release.publish(api, dist, TAG, SHA, CandidateSource())
            self.assertFalse(api.published)
            self.assertTrue(api.release["draft"])

    def test_existing_published_release_or_wrong_draft_is_not_modified(self):
        dist = self.candidates()
        for existing in [{"draft": False, "target_commitish": SHA, "tag_name": TAG, "id": 7},
                         {"draft": True, "target_commitish": "b" * 40, "tag_name": TAG, "id": 7}]:
            api = FakeApi(existing)
            with self.assertRaises(ValueError):
                release.publish(api, dist, TAG, SHA, CandidateSource())
            self.assertFalse(api.published)
            self.assertEqual(api.assets, {})

    def test_matching_partial_draft_can_resume_without_overwriting(self):
        dist = self.candidates()
        api = FakeApi({"draft": True, "target_commitish": SHA, "tag_name": TAG, "id": 7})
        archive = next(dist.glob("*.tar.gz"))
        api.assets[archive.name] = archive.read_bytes()
        release.publish(api, dist, TAG, SHA, CandidateSource())
        self.assertTrue(api.published)

    def test_matching_draft_beyond_first_release_page_is_resumed(self):
        dist = self.candidates()
        api = FakeApi({"draft": True, "target_commitish": SHA, "tag_name": TAG, "id": 7})
        api.history = [{"tag_name": f"v0.0.{index}", "draft": False} for index in range(100)]
        archive = next(dist.glob("*.tar.gz"))
        api.assets[archive.name] = archive.read_bytes()
        release.publish(api, dist, TAG, SHA, CandidateSource())
        self.assertTrue(api.published)
        self.assertEqual(api.release["id"], 7)

    def test_missing_dedicated_destination_or_token_cannot_publish(self):
        for configuration in [
            {"GITHUB_TOKEN": "source-only", "GITHUB_REPOSITORY": REPOSITORY,
             "GITHUB_SERVER_URL": "https://forgejo.example.org"},
            {"KALISTO_GITHUB_TOKEN": "dedicated"},
            {"KALISTO_GITHUB_REPOSITORY": REPOSITORY},
        ]:
            with patch.dict(os.environ, dict(configuration, GITHUB_EVENT_NAME="push",
                                             GITHUB_REF=f"refs/tags/{TAG}"), clear=True), \
                    patch("sys.argv", ["release.py", "publish", "--root", str(self.root),
                                       "--tag", TAG, "--commit", SHA]):
                with self.assertRaises(ValueError):
                    release.main()

    def test_pr_manual_or_wrong_tag_context_cannot_publish(self):
        for event, ref in [("pull_request", f"refs/tags/{TAG}"),
                           ("workflow_dispatch", f"refs/tags/{TAG}"),
                           ("push", "refs/heads/main")]:
            with patch.dict(os.environ, {"GITHUB_EVENT_NAME": event, "GITHUB_REF": ref}), \
                    patch("sys.argv", ["release.py", "publish", "--root", str(self.root),
                                       "--tag", TAG, "--commit", SHA]):
                with self.assertRaisesRegex(ValueError, "only for a pushed"):
                    release.main()

    def test_different_or_unexpected_draft_assets_are_preserved(self):
        dist = self.candidates()
        for name in [next(dist.glob("*.tar.gz")).name, "unexpected"]:
            api = FakeApi({"draft": True, "target_commitish": SHA, "tag_name": TAG, "id": 7})
            api.assets[name] = b"operator data"
            with self.assertRaises(ValueError):
                release.publish(api, dist, TAG, SHA, CandidateSource())
            self.assertEqual(api.assets[name], b"operator data")
            self.assertFalse(api.published)


class GitHubApiTests(unittest.TestCase):
    def setUp(self):
        self.api = release.GitHub("dedicated", REPOSITORY)

    def response(self, content):
        response = unittest.mock.MagicMock()
        response.__enter__.return_value.read.return_value = content
        return response

    def test_destination_is_explicit_repository_and_dedicated_token(self):
        for repository in ["", "kalisto", "../kalisto", "example/..", "example/kalisto/extra"]:
            with self.assertRaises(ValueError):
                release.GitHub("dedicated", repository)
        with self.assertRaises(ValueError):
            release.GitHub("", REPOSITORY)
        self.assertEqual(self.api.base, f"https://api.github.com/repos/{REPOSITORY}")

    def test_api_reads_and_binary_asset_upload_use_github_protocol(self):
        requests = []
        def open_request(request, timeout):
            requests.append(request)
            return self.response(b'{"id": 7}')
        with patch("urllib.request.OpenerDirector.open", side_effect=open_request):
            self.assertEqual(self.api.request("GET", "/releases/7"), {"id": 7})
            self.api.upload({"id": 7, "upload_url":
                             f"https://uploads.github.com/repos/{REPOSITORY}/releases/7/assets{{?name,label}}"},
                            "archive.tar.gz", b"raw archive bytes")
        self.assertEqual(requests[0].full_url, f"{self.api.base}/releases/7")
        self.assertEqual(requests[0].get_header("Authorization"), "Bearer dedicated")
        self.assertEqual(requests[1].full_url,
                         f"https://uploads.github.com/repos/{REPOSITORY}/releases/7/assets?name=archive.tar.gz")
        self.assertEqual(requests[1].data, b"raw archive bytes")
        self.assertEqual(requests[1].get_header("Content-type"), "application/octet-stream")

    def test_asset_download_uses_authenticated_asset_api_and_checks_size(self):
        url = f"{self.api.base}/releases/assets/7"
        with patch("urllib.request.OpenerDirector.open", return_value=self.response(b"archive")) as opened:
            self.assertEqual(self.api.download({"url": url, "size": 7,
                                               "browser_download_url": "https://evil.example/"}), b"archive")
            request = opened.call_args.args[0]
            self.assertEqual(request.full_url, url)
            self.assertEqual(request.get_header("Authorization"), "Bearer dedicated")
            self.assertEqual(request.get_header("Accept"), "application/octet-stream")
            for size in [0, 8]:
                with self.assertRaises(ValueError):
                    self.api.download({"url": url, "size": size})
        for url in ["https://evil.example/asset", f"{SERVER}/repos/other/repo/releases/assets/7",
                    f"{self.api.base}/releases/assets/7?redirect=evil",
                    f"{self.api.base}/releases/assets/../7"]:
            with self.assertRaises(ValueError):
                self.api.download({"url": url, "size": 0})

    def test_signed_asset_redirect_strips_auth_and_cannot_return_to_credentialed_api(self):
        redirect = release.GitHubRedirect(asset=True)
        request = urllib.request.Request(f"{self.api.base}/releases/assets/7",
                                         headers={"Authorization": "Bearer dedicated",
                                                  "Accept": "application/octet-stream"})
        for host in ["release-assets.githubusercontent.com", "objects.githubusercontent.com"]:
            redirected = redirect.redirect_request(request, None, 302, "", {},
                                                    f"https://{host}/asset?signature=opaque")
            self.assertIsNone(redirected.get_header("Authorization"))
            self.assertEqual(redirected.get_header("Accept"), "application/octet-stream")
            with self.assertRaises(ValueError):
                redirect.redirect_request(redirected, None, 302, "", {}, request.full_url)

    def test_untrusted_redirects_fail_closed_for_assets_and_api_requests(self):
        request = urllib.request.Request(f"{self.api.base}/releases/assets/7",
                                         headers={"Authorization": "Bearer dedicated"})
        urls = ["https://evil.example/asset?signed=1", "http://release-assets.githubusercontent.com/a?s=1",
                "https://release-assets.githubusercontent.com/a", "https://github.com/a?s=1",
                "https://user@objects.githubusercontent.com/a?s=1",
                "https://objects.githubusercontent.com:443/a?s=1",
                "https://objects.githubusercontent.com.evil.example/a?s=1",
                "https://raw.githubusercontent.com/a?s=1"]
        for url in urls:
            with self.assertRaises(ValueError, msg=url):
                release.GitHubRedirect(asset=True).redirect_request(request, None, 302, "", {}, url)
        with self.assertRaises(ValueError):
            release.GitHubRedirect().redirect_request(request, None, 302, "", {},
                "https://release-assets.githubusercontent.com/a?s=1")

    def test_untrusted_upload_url_never_receives_credentials(self):
        for url in ["https://evil.example/assets", "http://uploads.github.com/assets",
                    f"https://uploads.github.com/repos/other/repo/releases/7/assets"]:
            with self.assertRaises(ValueError):
                self.api.upload({"id": 7, "upload_url": url}, "archive", b"bytes")

    def test_exact_singular_ref_resolves_annotated_tags(self):
        requests = []
        def open_request(request, timeout):
            requests.append(request.full_url)
            if "/git/ref/tags/" in request.full_url:
                return self.response(json.dumps({"ref": f"refs/tags/{TAG}", "object": {
                    "type": "tag", "sha": "d" * 40, "url": "ignored"}}).encode())
            return self.response(json.dumps({"object": {"type": "commit", "sha": SHA}}).encode())
        with patch("urllib.request.OpenerDirector.open", side_effect=open_request):
            self.assertEqual(self.api.tag_commit(TAG), SHA)
        self.assertEqual(requests, [f"{self.api.base}/git/ref/tags/{TAG}",
                                   f"{self.api.base}/git/tags/{'d' * 40}"])

    def test_wrong_ref_or_non_commit_tag_is_rejected(self):
        for ref in [
            {"ref": "refs/tags/v1.2.30", "object": {"type": "commit", "sha": SHA}},
            {"ref": f"refs/tags/{TAG}", "object": {"type": "tree", "sha": SHA}},
        ]:
            with patch("urllib.request.OpenerDirector.open",
                       return_value=self.response(json.dumps(ref).encode())):
                with self.assertRaises(ValueError):
                    self.api.tag_commit(TAG)

    def test_github_provenance_requires_same_tag_object_commit_and_main_ancestry(self):
        expected = {"type": "tag", "sha": "d" * 40}
        for status in ["ahead", "identical", "behind", "diverged"]:
            with patch.object(self.api, "tag_object", return_value=expected), \
                    patch.object(self.api, "tag_commit", return_value=SHA), \
                    patch.object(self.api, "request", side_effect=[
                        {"sha": SHA}, {"status": status}]):
                if status in ("ahead", "identical"):
                    self.api.verify_source(TAG, SHA, expected)
                else:
                    with self.assertRaises(ValueError):
                        self.api.verify_source(TAG, SHA, expected)
        # A new annotation for the same peeled commit is still a changed tag.
        with patch.object(self.api, "tag_object", return_value={"type": "tag", "sha": "e" * 40}), \
                patch.object(self.api, "tag_commit", return_value=SHA):
            with self.assertRaises(ValueError):
                self.api.verify_source(TAG, SHA, expected)


class SourceProvenanceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        previous = Path.cwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, previous)
        config = self.root / "gitconfig"
        config.write_text(
            f'[url "file://{self.root}/github.git"]\n'
            f'\tinsteadOf = https://github.com/{REPOSITORY}.git\n'
            f'[url "file://{self.root}/source.git"]\n'
            f'\tinsteadOf = https://forgejo.example.org/{REPOSITORY}.git\n')
        environment = patch.dict(os.environ, {
            "GIT_CONFIG_GLOBAL": str(config), "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Release test", "GIT_AUTHOR_EMAIL": "test@example.org",
            "GIT_COMMITTER_NAME": "Release test", "GIT_COMMITTER_EMAIL": "test@example.org",
            "GITHUB_SERVER_URL": "https://forgejo.example.org",
            "GITHUB_REPOSITORY": REPOSITORY, "GITHUB_TOKEN": "source-only"})
        environment.start()
        self.addCleanup(environment.stop)
        self.git("init", "-q", "--bare", "source.git")
        self.git("init", "-q", "--bare", "github.git")
        self.git("init", "-q", "-b", "main", "checkout")
        os.chdir(self.root / "checkout")
        self.git("commit", "-q", "--allow-empty", "-m", "Reviewed main")
        self.commit = self.git("rev-parse", "HEAD")
        self.git("tag", "-a", "-m", "Reviewed release", TAG)
        self.tag_sha = self.git("rev-parse", f"refs/tags/{TAG}")
        self.git("remote", "add", "origin", str(self.root / "source.git"))
        self.git("push", "-q", "origin", "main", f"refs/tags/{TAG}")
        self.source = release.SourceRepository()
        self.api = release.GitHub("dedicated", REPOSITORY)
        self.verification = patch.object(self.api, "verify_source")
        self.verification.start()
        self.addCleanup(self.verification.stop)

    def git(self, *args):
        return subprocess.run(["git", *args], check=True, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True).stdout.strip()

    def github_ref(self, ref):
        return self.git("--git-dir", str(self.root / "github.git"), "rev-parse", ref)

    def test_syncs_exact_annotated_tag_and_reviewed_main_without_rewriting(self):
        expected = self.source.prepare(self.api, TAG, self.commit)
        self.assertEqual(expected, {"type": "tag", "sha": self.tag_sha})
        self.assertEqual(self.github_ref(f"refs/tags/{TAG}"), self.tag_sha)
        self.assertEqual(self.github_ref(f"refs/tags/{TAG}^{{commit}}"), self.commit)
        self.assertEqual(self.github_ref("refs/heads/main"), self.commit)
        self.source.prepare(self.api, TAG, self.commit)
        self.assertEqual(self.github_ref(f"refs/tags/{TAG}"), self.tag_sha)

    def test_behind_github_main_is_fast_forwarded_only_to_the_tagged_commit(self):
        self.git("push", "-q", str(self.root / "github.git"), "main")
        self.git("commit", "-q", "--allow-empty", "-m", "Tagged successor")
        tagged = self.git("rev-parse", "HEAD")
        self.git("tag", "-a", "-m", "Successor release", "v2.0.0")
        self.git("commit", "-q", "--allow-empty", "-m", "Later source work")
        self.git("push", "-q", "origin", "main", "refs/tags/v2.0.0")
        self.git("checkout", "-q", "--detach", tagged)
        self.source.prepare(self.api, "v2.0.0", tagged)
        self.assertEqual(self.github_ref("refs/heads/main"), tagged)
        self.assertEqual(self.github_ref("refs/tags/v2.0.0^{commit}"), tagged)

    def test_tagged_ancestor_does_not_publish_later_source_main_changes(self):
        self.git("commit", "-q", "--allow-empty", "-m", "Next reviewed main")
        self.git("push", "-q", "origin", "main")
        self.git("checkout", "-q", "--detach", self.commit)
        self.source.prepare(self.api, TAG, self.commit)
        self.assertEqual(self.github_ref("refs/heads/main"), self.commit)
        self.assertEqual(self.github_ref(f"refs/tags/{TAG}^{{commit}}"), self.commit)

    def test_wrong_tagged_source_or_unreviewed_commit_never_syncs(self):
        self.git("commit", "-q", "--allow-empty", "-m", "Unreviewed branch")
        unreviewed = self.git("rev-parse", "HEAD")
        with self.assertRaises(ValueError):
            self.source.prepare(self.api, TAG, unreviewed)
        self.git("tag", "-a", "-m", "Unreviewed release", "v2.0.0")
        self.git("push", "-q", "origin", "refs/tags/v2.0.0")
        with self.assertRaises(ValueError):
            self.source.prepare(self.api, "v2.0.0", unreviewed)
        self.assertEqual(self.git("ls-remote", "--refs", str(self.root / "github.git")), "")

    def test_existing_different_annotation_for_same_commit_is_never_replaced(self):
        self.git("tag", "-a", "-m", "Different annotation", "other")
        other = self.git("rev-parse", "refs/tags/other")
        self.git("push", "-q", str(self.root / "github.git"), f"{other}:refs/tags/{TAG}")
        with self.assertRaisesRegex(ValueError, "different tag"):
            self.source.prepare(self.api, TAG, self.commit)
        self.assertEqual(self.github_ref(f"refs/tags/{TAG}"), other)

    def test_divergent_github_main_is_not_overwritten_or_given_a_tag(self):
        self.git("checkout", "-q", "--orphan", "divergent")
        self.git("commit", "-q", "--allow-empty", "-m", "Independent GitHub main")
        divergent = self.git("rev-parse", "HEAD")
        self.git("push", "-q", str(self.root / "github.git"), f"{divergent}:refs/heads/main")
        self.git("checkout", "-q", "main")
        with self.assertRaises(ValueError):
            self.source.prepare(self.api, TAG, self.commit)
        self.assertEqual(self.github_ref("refs/heads/main"), divergent)
        self.assertFalse((self.root / "github.git/refs/tags" / TAG).exists())

    def test_ahead_github_main_is_preserved_while_the_release_tag_is_synced(self):
        self.git("commit", "-q", "--allow-empty", "-m", "GitHub-only successor")
        ahead = self.git("rev-parse", "HEAD")
        self.git("push", "-q", str(self.root / "github.git"), f"{ahead}:refs/heads/main")
        self.git("checkout", "-q", "--detach", self.commit)
        self.source.prepare(self.api, TAG, self.commit)
        self.assertEqual(self.github_ref("refs/heads/main"), ahead)
        self.assertEqual(self.github_ref(f"refs/tags/{TAG}"), self.tag_sha)

    def test_protected_main_rejection_is_atomic_and_does_not_publish_source_tag(self):
        hook = self.root / "github.git/hooks/pre-receive"
        hook.write_text("#!/bin/sh\necho 'branch protection rejected dedicated' >&2\nexit 1\n")
        hook.chmod(0o755)
        with self.assertRaises(ValueError) as error:
            self.source.prepare(self.api, TAG, self.commit)
        self.assertNotIn("dedicated", str(error.exception))
        self.assertFalse((self.root / "github.git/refs/tags" / TAG).exists())
        self.assertFalse((self.root / "github.git/refs/heads/main").exists())

    def test_source_tag_changed_after_sync_is_rejected_even_with_same_peeled_commit(self):
        expected = self.source.prepare(self.api, TAG, self.commit)
        self.git("--git-dir", str(self.root / "source.git"), "update-ref",
                 f"refs/tags/{TAG}", self.commit)
        with self.assertRaisesRegex(ValueError, "source release tag changed"):
            self.source.verify(self.api, TAG, self.commit, expected)


    def test_source_workflow_token_cannot_be_used_as_a_github_fallback(self):
        with patch.dict(os.environ, {"GITHUB_SERVER_URL": "https://github.com"}):
            with self.assertRaisesRegex(ValueError, "source checkout credentials"):
                self.source.prepare(self.api, TAG, self.commit)
        self.assertEqual(self.git("ls-remote", "--refs", str(self.root / "github.git")), "")


if __name__ == "__main__":
    unittest.main()
