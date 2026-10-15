import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


CHECKER_PATH = Path(__file__).with_name("check-network-hosts.py")
SPEC = importlib.util.spec_from_file_location("check_network_hosts", CHECKER_PATH)
if SPEC is None or SPEC.loader is None:
    raise ImportError(f"cannot load checker from {CHECKER_PATH}")
CHECKER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHECKER
SPEC.loader.exec_module(CHECKER)


class NetworkHostsTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.root = Path(self.temp_dir.name)
        (self.root / "scripts").mkdir()
        (self.root / "src").mkdir()
        (self.root / "src-tauri" / "src").mkdir(parents=True)

    def write_manifest(self, hosts=None, local_hosts=None):
        manifest = {
            "hosts": hosts or {},
            "local_hosts": local_hosts or [],
        }
        path = self.root / "scripts" / "network-hosts.json"
        path.write_text(json.dumps(manifest), encoding="utf-8")

    def write_source(self, relative_path, content):
        path = self.root / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def check(self):
        return CHECKER.check(self.root)

    def test_rejects_unregistered_domain(self):
        self.write_manifest()
        self.write_source(
            "src-tauri/src/client.rs",
            'const API: &str = "https://api.unlisted-vendor.com/v1";\n',
        )

        errors = self.check()

        self.assertEqual(
            errors,
            ["src-tauri/src/client.rs:1: unregistered hard-coded URL host"],
        )

    def test_accepts_registered_domain(self):
        self.write_manifest(
            hosts={"api.vendor-example.com": {"purpose": "provider API"}}
        )
        self.write_source(
            "src/client.ts",
            'export const API = "https://api.vendor-example.com/v1";\n',
        )

        self.assertEqual(self.check(), [])

    def test_files_scope_does_not_authorize_other_production_sources(self):
        self.write_manifest(
            hosts={
                "api.vendor-example.com": {
                    "purpose": "provider API",
                    "files": ["src/client.ts"],
                }
            }
        )
        url = "https://api.vendor-example.com/v1"
        self.write_source("src/client.ts", f'const API = "{url}";\n')
        self.write_source(
            "src-tauri/src/client.rs",
            f'const API: &str = "{url}";\n',
        )

        self.assertEqual(
            self.check(),
            ["src-tauri/src/client.rs:1: unregistered hard-coded URL host"],
        )

    def test_error_does_not_echo_url_userinfo_or_query(self):
        self.write_manifest()
        self.write_source(
            "src/client.ts",
            'const API = "https://fixture-user:fixture-password@'
            'api.unlisted-vendor.com/v1?fixture-query=fixture-value";\n',
        )

        errors = self.check()
        rendered = "\n".join(errors)

        self.assertEqual(
            errors,
            ["src/client.ts:1: unregistered hard-coded URL host"],
        )
        for fixture_value in (
            "fixture-user",
            "fixture-password",
            "fixture-query",
            "fixture-value",
        ):
            self.assertNotIn(fixture_value, rendered)

    def test_accepts_test_and_loopback_hosts_without_registration(self):
        self.write_manifest()
        self.write_source(
            "src/client.ts",
            "\n".join(
                [
                    'const invalid = "https://example.invalid/v1";',
                    'const test = "https://api.example.test/v1";',
                    'const local = "http://localhost:15722/v1";',
                    'const loopback = "http://127.0.0.1:15722/v1";',
                ]
            ),
        )

        self.assertEqual(self.check(), [])

    def test_allows_supplier_url_built_from_runtime_base_url(self):
        self.write_manifest()
        self.write_source(
            "src-tauri/src/gateway/request.rs",
            'fn endpoint(base_url: &str) -> String {\n'
            '    format!("{base_url}/v1/responses")\n'
            '}\n',
        )

        self.assertEqual(self.check(), [])


if __name__ == "__main__":
    unittest.main()
