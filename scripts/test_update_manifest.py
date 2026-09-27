#!/usr/bin/env python3
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
UPDATE_MANIFEST = SCRIPT_DIR / "update_manifest.py"
SIGN_MANIFEST = SCRIPT_DIR / "sign_update_manifest.py"


def run(command, env=None, check=True):
    return subprocess.run(command, env=env, check=check, capture_output=True, text=True)


@unittest.skipUnless(shutil.which("openssl"), "OpenSSL is required to run this test")
class UpdateProtocolTest(unittest.TestCase):
    def test_generate_sign_verify_and_reject_wrong_key(self):
        with tempfile.TemporaryDirectory(prefix="k8s-gpui-update-test-") as temporary:
            root = Path(temporary)
            app = root / "k8s-app"
            app.write_bytes(b"app")
            app.chmod(0o755)
            key = root / "signing-key.pem"
            subprocess.run(
                ["openssl", "genpkey", "-algorithm", "ED25519", "-out", str(key)],
                check=True,
                capture_output=True,
            )
            der = subprocess.check_output(
                ["openssl", "pkey", "-in", str(key), "-pubout", "-outform", "DER"]
            )
            public = der[-32:].hex()
            environment = os.environ.copy()
            environment["K8S_GPUI_UPDATE_PUBLIC_KEY"] = public
            environment["K8S_GPUI_UPDATE_SIGNING_KEY"] = key.read_text()
            manifest = root / "update-linux-x86_64.json"
            signature = root / "update-linux-x86_64.json.sig"
            run(
                [
                    sys.executable,
                    str(UPDATE_MANIFEST),
                    "generate",
                    "--output",
                    str(manifest),
                    "--repository",
                    "0x5457/k8s-gpui",
                    "--tag",
                    "v1.2.3",
                    "--version",
                    "1.2.3",
                    "--os",
                    "linux",
                    "--arch",
                    "x86_64",
                    "--artifact",
                    str(app),
                    "--published-at",
                    "2026-01-02T03:04:05Z",
                ]
            )
            value = json.loads(manifest.read_text())
            self.assertEqual(
                list(value),
                ["schema", "channel", "version", "published_at", "artifacts"],
            )
            self.assertEqual(value["schema"], 1)
            self.assertEqual(value["channel"], "stable")
            self.assertEqual(
                value["artifacts"][0]["url"],
                "https://github.com/0x5457/k8s-gpui/releases/download/v1.2.3/k8s-app",
            )
            self.assertEqual(value["artifacts"][0]["size"], 3)
            self.assertEqual(len(value["artifacts"]), 1)
            run([sys.executable, str(SIGN_MANIFEST), "check-key"], env=environment)
            run(
                [
                    sys.executable,
                    str(SIGN_MANIFEST),
                    "sign",
                    "--manifest",
                    str(manifest),
                    "--signature",
                    str(signature),
                ],
                env=environment,
            )
            self.assertEqual(signature.stat().st_size, 64)
            run(
                [
                    sys.executable,
                    str(SIGN_MANIFEST),
                    "verify",
                    "--manifest",
                    str(manifest),
                    "--signature",
                    str(signature),
                ],
                env=environment,
            )
            run(
                [
                    sys.executable,
                    str(UPDATE_MANIFEST),
                    "verify",
                    "--manifest",
                    str(manifest),
                    "--artifact-root",
                    str(root),
                    "--repository",
                    "0x5457/k8s-gpui",
                    "--tag",
                    "v1.2.3",
                    "--version",
                    "1.2.3",
                    "--os",
                    "linux",
                    "--arch",
                    "x86_64",
                    "--required-artifact",
                    "k8s-app",
                ]
            )
            wrong = environment.copy()
            wrong["K8S_GPUI_UPDATE_PUBLIC_KEY"] = "00" * 32
            result = run(
                [
                    sys.executable,
                    str(SIGN_MANIFEST),
                    "check-key",
                ],
                env=wrong,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn(key.read_text(), result.stderr)


if __name__ == "__main__":
    unittest.main()
