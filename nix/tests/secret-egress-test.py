import importlib.util
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True

source_module = Path(sys.argv[1]).resolve()
spec = importlib.util.spec_from_file_location("secret_egress", source_module)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class SecretEgressTests(unittest.TestCase):
    def test_host_projection_deduplicates_and_preserves_normal_sets(self):
        self.assertEqual(module.dns_config([], "192.0.2.53"), "")
        text = module.dns_config(["api.example.com", "api.example.com"], "2001:db8::53")
        self.assertEqual(text.count("server="), 1)
        self.assertIn("server=/api.example.com/2001:db8::53", text)
        self.assertIn("#broker4", text)
        self.assertIn("#broker6", text)
        text = module.dns_config(["api.example.com"], "192.0.2.53", [
            {"name": "web", "domains": ["example.com"]},
            {"name": "rule0", "domains": ["other.example.com"]}])
        self.assertIn("#web4", text)
        self.assertIn("#web6", text)
        self.assertNotIn("#rule0", text)

    def test_projection_rejects_injection_and_address_literals(self):
        for host in ["*.example.com", "api.example.com\nserver=/#/1.1.1.1", "127.0.0.1",
                     "api.example.com:443", "-a.example", "a..example"]:
            with self.subTest(host=host), self.assertRaises(ValueError):
                module.dns_config([host], "192.0.2.53")

    def test_removals_stop_dns_before_flushing_stale_addresses(self):
        with tempfile.TemporaryDirectory() as folder:
            folder = Path(folder)
            source, target = folder / "hosts.json", folder / "dnsmasq.conf"
            source.write_text('["api.example.com"]')
            target.write_text("old")
            with patch.object(module.subprocess, "run") as run:
                def before_flush(args, **kwargs):
                    if args[0] == "nft":
                        self.assertEqual(target.read_text(), "old",
                                         "configuration published before stale addresses were removed")
                run.side_effect = before_flush
                module.reconcile(source, target, "192.0.2.53")
                run.side_effect = None
                self.assertIn("api.example.com", target.read_text())
                self.assertEqual(run.call_args_list[0].args[0], ["systemctl", "stop", "dnsmasq.service"])
                self.assertIn("flush set", run.call_args_list[1].kwargs["input"])
                self.assertEqual(run.call_args_list[2].args[0], ["systemctl", "start", "dnsmasq.service"])
                source.write_text("[]")
                module.reconcile(source, target, "192.0.2.53")
                self.assertEqual(target.read_text(), "")

    def test_nonregular_descriptor_is_rejected_before_root_actions(self):
        with tempfile.TemporaryDirectory() as folder:
            folder = Path(folder)
            source, target = folder / "hosts.json", folder / "dnsmasq.conf"
            source.write_text('["api.example.com"]')
            target.write_text("old")
            # A regular backing file makes this test bounded even if fstat is accidentally
            # removed. Its reported type must be checked on the opened descriptor.
            for mode in [stat.S_IFIFO, stat.S_IFDIR, stat.S_IFCHR, stat.S_IFSOCK]:
                with self.subTest(mode=mode), patch.object(module.os, "fstat", return_value=SimpleNamespace(st_mode=mode)), patch.object(module.subprocess, "run") as run:
                    with self.assertRaisesRegex(ValueError, "regular file"):
                        module.reconcile(source, target, "192.0.2.53")
                    run.assert_not_called()
                    self.assertEqual(target.read_text(), "old")

    def test_fifo_without_writer_never_blocks_root_reconciliation(self):
        with tempfile.TemporaryDirectory() as folder:
            folder = Path(folder)
            source, target = folder / "hosts.json", folder / "dnsmasq.conf"
            os.mkfifo(source)
            target.write_text("old")
            # A subprocess timeout catches a regression to a blocking open without hanging
            # the whole check. No writer ever opens the FIFO.
            result = subprocess.run([sys.executable, str(source_module), str(source), str(target), "192.0.2.53"],
                                    capture_output=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, b"")
            self.assertEqual(result.stderr, b"secret broker network reconciliation failed (metadata hidden)\n")
            self.assertEqual(target.read_text(), "old")

    def test_symlinks_and_oversized_files_do_not_change_resolver_state(self):
        with tempfile.TemporaryDirectory() as folder:
            folder = Path(folder)
            regular, link, target = folder / "hosts.json", folder / "link", folder / "dnsmasq.conf"
            regular.write_text('["api.example.com"]')
            link.symlink_to(regular)
            target.write_text("old")
            with patch.object(module.subprocess, "run") as run:
                with self.assertRaises(OSError):
                    module.reconcile(link, target, "192.0.2.53")
                regular.write_bytes(b" " * 65537)
                with self.assertRaisesRegex(ValueError, "too large"):
                    module.reconcile(regular, target, "192.0.2.53")
                run.assert_not_called()
                self.assertEqual(target.read_text(), "old")


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]], verbosity=2)
