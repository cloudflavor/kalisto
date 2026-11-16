"""Host-safe sanitizer regressions; these never run Kalisto or record a scene."""
import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from capture import CaptureError, privacy_gate, publish_cast, redact_events


class PublicationPrivacyTests(unittest.TestCase):
    def test_private_path_at_every_possible_pty_boundary(self):
        original = "kubeconfig: /Users/private-person/demo/data/kubeconfig.yaml\r\n"
        expected = "kubeconfig: <demo-workspace>/data/kubeconfig.yaml\r\n"
        replacements = {"/Users/private-person/demo": "<demo-workspace>",
                        "/Users/private-person": "<runner-home>"}
        for boundary in range(1, len(original)):
            with self.subTest(boundary=boundary):
                events = [[0.125, "o", original[:boundary]], [8.75, "o", original[boundary:]]]
                result = redact_events(events, replacements)
                self.assertEqual("".join(event[2] for event in result), expected)
                self.assertEqual([event[:2] for event in result], [[0.125, "o"], [8.75, "o"]])
                privacy_gate("".join(event[2] for event in result), ["private-person"])

    def test_multiple_spanning_replacements_preserve_all_events(self):
        events = [[0.1, "o", "/Users/"], [1.2, "o", "private-person/demo"],
                  [4.5, "o", "\n/Users/private-"], [7.8, "o", "person/demo/end\n"]]
        result = redact_events(events, {"/Users/private-person/demo": "<demo-workspace>"})
        self.assertEqual("".join(event[2] for event in result), "<demo-workspace>\n<demo-workspace>/end\n")
        self.assertEqual([event[0] for event in result], [0.1, 1.2, 4.5, 7.8])
        self.assertEqual(len(result), 4)
        self.assertEqual(result[1][2], "")

    def test_split_identity_and_credentials_are_rejected_after_join(self):
        for fragments in (("private-", "person"), ("ci.private.", "internal"),
                          ("client-key-", "data: abc"), ("https://ci.", "example.invalid/path")):
            with self.subTest(fragments=fragments):
                with self.assertRaises(CaptureError):
                    privacy_gate("".join(fragments), ["private-person"])

    def test_public_cast_keeps_timing_and_replaces_incidental_header(self):
        with tempfile.TemporaryDirectory() as directory:
            raw, target = Path(directory) / "raw.cast", Path(directory) / "start.cast"
            header = {"version": 2, "width": 110, "height": 30, "timestamp": 1700000000,
                      "env": {"USER": "private-person"}, "command": "/Users/private-person/script.py",
                      "idle_time_limit": 1}
            events = [[0.123456, "o", "/Users/private-"], [11.234567, "o", "person/demo/output\r\n"]]
            raw.write_text("\n".join(json.dumps(row) for row in [header, *events]) + "\n")
            metadata = publish_cast(raw, target, "start", {"/Users/private-person/demo": "<demo-workspace>"},
                                    ["private-person"])
            published = [json.loads(line) for line in target.read_text().splitlines()]
            self.assertEqual([row[0] for row in published[1:]], [0.123456, 11.234567])
            self.assertEqual(published[0]["timestamp"], header["timestamp"])
            self.assertEqual(published[0]["command"], "python3 scene.py start")
            self.assertNotIn("idle_time_limit", published[0])
            self.assertNotIn("USER", published[0]["env"])
            self.assertEqual(metadata["duration_seconds"], 11.234567)
            self.assertEqual(metadata["sha256"], hashlib.sha256(target.read_bytes()).hexdigest())
            self.assertNotIn("private-person", target.read_text())


if __name__ == "__main__":
    unittest.main()
