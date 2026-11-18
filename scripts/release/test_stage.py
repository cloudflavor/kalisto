import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import release
import stage
from test_release import FakeApi, binary, SHA, TAG


class StageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.cache = self.root / "cache" / TAG
        for target in release.TARGETS:
            source = self.root / target
            source.write_bytes(binary(target))
            release.package(source, self.root / "dist", TAG, "1.2.3", SHA, target)
        self.api = FakeApi()
        release.publish(self.api, self.root / "dist", TAG, SHA)
        self.api.tag_reads = 0

    def test_stages_exact_published_bytes_and_reuses_identical_cache(self):
        stage.stage(self.api, self.cache, TAG, SHA)
        for name, data in self.api.assets.items():
            self.assertEqual((self.cache / name).read_bytes(), data)
        stage.stage(self.api, self.cache, TAG, SHA)
        self.assertEqual(set(release.collect_artifacts(self.cache, TAG, SHA)),
                         set(self.api.assets) - {"SHA256SUMS"})

    def test_cli_stages_a_published_tag_independent_of_workflow_checkout(self):
        with patch("sys.argv", ["stage.py", "--root", str(self.cache), "--tag", TAG]), \
                patch.dict(os.environ, {"GITHUB_SHA": "b" * 40}), \
                patch("stage.Forgejo", return_value=self.api), patch("builtins.print"):
            stage.main()
        self.assertEqual(set(release.collect_artifacts(self.cache, TAG, SHA)),
                         set(self.api.assets) - {"SHA256SUMS"})

    def test_refuses_draft_or_wrong_source_without_creating_cache(self):
        for changes in ({"draft": True}, {"target_commitish": "b" * 40}):
            with self.subTest(changes=changes):
                original = dict(self.api.release)
                self.api.release.update(changes)
                with self.assertRaisesRegex(ValueError, "only a published release"):
                    stage.stage(self.api, self.cache, TAG, SHA)
                self.assertFalse(self.cache.exists())
                self.api.release = original

    def test_rejects_corrupt_checksums_before_exposing_downloads(self):
        self.api.assets["SHA256SUMS"] = b"incorrect\n"
        with self.assertRaisesRegex(ValueError, "checksums"):
            stage.stage(self.api, self.cache, TAG, SHA)
        self.assertFalse(self.cache.exists())

    def test_rejects_missing_archive_before_exposing_downloads(self):
        del self.api.assets[next(name for name in self.api.assets if name.endswith(".tar.gz"))]
        with self.assertRaisesRegex(ValueError, "inventory"):
            stage.stage(self.api, self.cache, TAG, SHA)
        self.assertFalse(self.cache.exists())

    def test_refuses_to_replace_different_cached_download(self):
        stage.stage(self.api, self.cache, TAG, SHA)
        filename = next(name for name in self.api.assets if name.endswith(".tar.gz"))
        (self.cache / filename).write_bytes(b"existing operator data")
        with self.assertRaisesRegex(ValueError, "refusing to replace"):
            stage.stage(self.api, self.cache, TAG, SHA)
        self.assertEqual((self.cache / filename).read_bytes(), b"existing operator data")

    def test_refuses_symlinked_cache_without_touching_target(self):
        target = self.root / "operator-data"
        target.mkdir()
        self.cache.parent.mkdir()
        self.cache.symlink_to(target, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlinked"):
            stage.stage(self.api, self.cache, TAG, SHA)
        self.assertEqual(list(target.iterdir()), [])

    def test_rejects_tag_moved_during_download_before_exposing_cache(self):
        self.api.moved = True
        with self.assertRaisesRegex(ValueError, "tag moved"):
            stage.stage(self.api, self.cache, TAG, SHA)
        self.assertFalse(self.cache.exists())


if __name__ == "__main__":
    unittest.main()
