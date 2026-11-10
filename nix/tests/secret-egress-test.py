import importlib.util
from pathlib import Path
import sys
import tempfile
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("secret_egress", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

assert module.dns_config([], "192.0.2.53") == ""
text = module.dns_config(["api.example.com", "api.example.com"], "2001:db8::53")
assert text.count("server=") == 1
assert "server=/api.example.com/2001:db8::53" in text
assert "#broker4" in text and "#broker6" in text
text = module.dns_config(["api.example.com"], "192.0.2.53", [
    {"name": "web", "domains": ["example.com"]},
    {"name": "rule0", "domains": ["other.example.com"]}])
assert "#web4" in text and "#web6" in text and "#rule0" not in text
for bad in ["*.example.com", "api.example.com\nserver=/#/1.1.1.1", "127.0.0.1", "api.example.com:443", "-a.example", "a..example"]:
    try:
        module.dns_config([bad], "192.0.2.53")
    except ValueError:
        pass
    else:
        raise AssertionError("invalid hostname accepted")
with tempfile.TemporaryDirectory() as folder:
    folder = Path(folder)
    source, target = folder / "hosts.json", folder / "dnsmasq.conf"
    source.write_text('["api.example.com"]')
    target.write_text("old")
    with patch.object(module.subprocess, "run") as run:
        def before_flush(args, **kwargs):
            if args[0] == "nft":
                assert target.read_text() == "old", "configuration published before stale addresses were removed"
        run.side_effect = before_flush
        module.reconcile(source, target, "192.0.2.53")
        run.side_effect = None
        assert "api.example.com" in target.read_text()
        assert run.call_args_list[0].args[0] == ["systemctl", "stop", "dnsmasq.service"]
        assert "flush set" in run.call_args_list[1].kwargs["input"]
        assert run.call_args_list[2].args[0] == ["systemctl", "start", "dnsmasq.service"]
        source.write_text("[]")
        module.reconcile(source, target, "192.0.2.53")
        assert target.read_text() == ""
