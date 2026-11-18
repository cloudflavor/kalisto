import copy
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import urllib.request

import release

SHA = "a" * 40
TAG = "v1.2.3"
SERVER = "https://forgejo.example.org"
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

    def tag_commit(self, tag):
        self.tag_reads += 1
        return "b" * 40 if self.moved and self.tag_reads > 1 else SHA

    def request(self, method, path, payload=None):
        if path.startswith("/releases/tags/"):
            if self.release is None:
                raise release.ApiError(404)
            return self.release
        if method == "POST" and path == "/releases":
            assert payload["draft"]
            self.release = dict(payload, id=7)
            return self.release
        if method == "GET" and path == "/releases/7/assets":
            return [{"name": name, "id": index, "size": len(data), "browser_download_url": f"{SERVER}/asset/{name}"}
                    for index, (name, data) in enumerate(self.assets.items())]
        if method == "PATCH":
            assert len(self.assets) == 5
            self.published = True
            self.release.update(payload)
            return self.release
        raise AssertionError((method, path))

    def upload(self, release_id, name, data):
        if self.fail_upload:
            raise release.ApiError(500)
        self.assets[name] = data

    def download(self, asset):
        return b"corrupt" if self.bad_readback else self.assets[asset["name"]]


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
        result = release.publish(api, self.candidates(), TAG, SHA)
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
        for flag in ["fail_upload", "bad_readback", "moved"]:
            api = FakeApi()
            setattr(api, flag, True)
            with self.assertRaises((ValueError, release.ApiError), msg=flag):
                release.publish(api, dist, TAG, SHA)
            self.assertFalse(api.published)
            self.assertTrue(api.release["draft"])

    def test_existing_published_release_or_wrong_draft_is_not_modified(self):
        dist = self.candidates()
        for existing in [{"draft": False, "target_commitish": SHA, "id": 7},
                         {"draft": True, "target_commitish": "b" * 40, "id": 7}]:
            api = FakeApi(existing)
            with self.assertRaises(ValueError):
                release.publish(api, dist, TAG, SHA)
            self.assertFalse(api.published)
            self.assertEqual(api.assets, {})

    def test_matching_partial_draft_can_resume_without_overwriting(self):
        dist = self.candidates()
        api = FakeApi({"draft": True, "target_commitish": SHA, "id": 7})
        archive = next(dist.glob("*.tar.gz"))
        api.assets[archive.name] = archive.read_bytes()
        release.publish(api, dist, TAG, SHA)
        self.assertTrue(api.published)

    def test_destination_requires_explicit_https_origin_repository_and_token(self):
        for server in ["", "http://example.com", "https://token@example.com",
                       "https://example.com/path", "https://example.com?query=1",
                       "https://example.com#fragment"]:
            with self.assertRaises(ValueError, msg=server):
                release.Forgejo("dummy", server, REPOSITORY)
        for repository in ["", "kalisto", "../kalisto", "example/..", "example/kalisto/extra"]:
            with self.assertRaises(ValueError, msg=repository):
                release.Forgejo("dummy", SERVER, repository)
        with self.assertRaises(ValueError):
            release.Forgejo("", SERVER, REPOSITORY)
        for server in [SERVER, "https://forgejo.example.org:8443/"]:
            api = release.Forgejo("dummy", server, REPOSITORY)
            self.assertEqual(api.base, f"{server.rstrip('/')}/api/v1/repos/{REPOSITORY}")

    def test_redirects_and_downloads_never_send_token_to_another_origin(self):
        api = release.Forgejo("dummy", SERVER, REPOSITORY)
        redirect = release.SameOriginRedirect(api.origin)
        for url in ["https://example.com/asset", "http://forgejo.example.org/asset",
                    "https://forgejo.example.org:8443/asset", "https://token@forgejo.example.org/asset"]:
            with self.assertRaises(ValueError, msg=url):
                redirect.redirect_request(None, None, 302, "", {}, url)
            with patch("urllib.request.build_opener") as opener:
                with self.assertRaises(ValueError, msg=url):
                    api.download({"browser_download_url": url, "size": 0})
                opener.assert_not_called()
        request = urllib.request.Request(f"{SERVER}/old")
        redirected = redirect.redirect_request(request, None, 302, "", {}, f"{SERVER}/new")
        self.assertEqual(redirected.full_url, f"{SERVER}/new")

    def test_configured_origin_is_used_for_requests_and_readback(self):
        api = release.Forgejo("dummy", "https://forgejo.example.org:8443", REPOSITORY)
        with patch("urllib.request.build_opener") as build_opener:
            response = build_opener.return_value.open.return_value.__enter__.return_value
            response.read.return_value = b'{"id": 7}'
            self.assertEqual(api.request("GET", "/releases/7"), {"id": 7})
            request = build_opener.return_value.open.call_args.args[0]
            self.assertEqual(request.full_url, f"{api.base}/releases/7")
            self.assertEqual(build_opener.call_args.args[0].origin, api.origin)
            response.read.return_value = b"archive"
            self.assertEqual(api.download({"browser_download_url": f"{api.origin}/asset",
                                           "size": 7}), b"archive")
            self.assertEqual(build_opener.call_args.args[0].origin, api.origin)

    def test_missing_workflow_destination_cannot_publish(self):
        with patch.dict("os.environ", {"GITHUB_EVENT_NAME": "push",
                                      "GITHUB_REF": f"refs/tags/{TAG}",
                                      "GITHUB_TOKEN": "dummy"}, clear=True), \
                patch("sys.argv", ["release.py", "publish", "--root", str(self.root),
                                   "--tag", TAG, "--commit", SHA]), \
                patch.object(release, "publish") as publish:
            with self.assertRaises(ValueError):
                release.main()
            publish.assert_not_called()

    def test_pushed_tag_uses_explicit_workflow_destination(self):
        environment = {"GITHUB_EVENT_NAME": "push", "GITHUB_REF": f"refs/tags/{TAG}",
                       "GITHUB_TOKEN": "dummy", "GITHUB_SERVER_URL": SERVER,
                       "GITHUB_REPOSITORY": REPOSITORY}
        with patch.dict("os.environ", environment, clear=True), \
                patch("sys.argv", ["release.py", "publish", "--root", str(self.root),
                                   "--tag", TAG, "--commit", SHA]), \
                patch.object(release, "Forgejo") as forgejo, \
                patch.object(release, "publish", return_value={"html_url": f"{SERVER}/release"}) as publish, \
                patch("builtins.print"):
            release.main()
            forgejo.assert_called_once_with("dummy", SERVER, REPOSITORY)
            publish.assert_called_once_with(forgejo.return_value, str(self.root), TAG, SHA)

    def test_pr_manual_or_wrong_tag_context_cannot_publish(self):
        for event, ref in [("pull_request", "refs/tags/v1.2.3"),
                           ("workflow_dispatch", "refs/tags/v1.2.3"),
                           ("push", "refs/heads/main")]:
            with patch.dict("os.environ", {"GITHUB_EVENT_NAME": event, "GITHUB_REF": ref}), \
                    patch("sys.argv", ["release.py", "publish", "--root", str(self.root), "--tag", TAG, "--commit", SHA]), \
                    patch.object(release, "publish") as publish:
                with self.assertRaises(ValueError):
                    release.main()
                publish.assert_not_called()

    def test_tag_api_filters_exact_ref_and_resolves_annotated_tag(self):
        api = release.Forgejo("dummy", SERVER, REPOSITORY)
        responses = [
            [{"ref": "refs/tags/v1.2.30", "object": {"type": "commit", "sha": "c" * 40}},
             {"ref": "refs/tags/v1.2.3", "object": {"type": "tag", "sha": "d" * 40}}],
            {"object": {"type": "commit", "sha": SHA}},
        ]
        with patch.object(api, "request", side_effect=responses):
            self.assertEqual(api.tag_commit(TAG), SHA)


if __name__ == "__main__":
    unittest.main()
