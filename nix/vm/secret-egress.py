"""Root's bounded metadata projection; never reads a secret or runs broker-supplied code."""
import ipaddress
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys


def dns_config(hosts, upstream, rules=()):
    ipaddress.ip_address(upstream)
    if not isinstance(hosts, list) or len(hosts) > 1024:
        raise ValueError("invalid broker host list")
    for host in hosts:
        if (not isinstance(host, str) or len(host) > 253 or "." not in host
                or any(not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", label)
                       for label in host.split("."))):
            raise ValueError("invalid broker host")
        try:
            ipaddress.ip_address(host)
        except ValueError:
            continue
        raise ValueError("address literals are not broker hosts")
    result = []
    for host in sorted(set(hosts)):
        names = ["broker"] + [rule["name"] for rule in rules
                              if any(host == domain or host.endswith("." + domain)
                                     for domain in rule["domains"])]
        sets = ",".join(f"{family}#inet#atomics_egress#{name}{family}"
                        for name in names for family in (4, 6))
        # Include the normal sets too: dnsmasq may prefer this more specific hostname to
        # a parent domain's nftset mapping. Adding a secret must not narrow agent egress.
        result += [f"server=/{host}/{upstream}\n", f"nftset=/{host}/{sets}\n"]
    return "".join(result)


def reconcile(source, target, upstream, policy=None):
    # Do not follow a link out of the broker's private state directory.
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise ValueError("broker host list must be a regular file")
        with os.fdopen(fd, "rb", closefd=False) as stream:
            data = stream.read(65537)
    finally:
        os.close(fd)
    if len(data) > 65536:
        raise ValueError("broker host list too large")
    rules = json.loads(Path(policy).read_text()) if policy else []
    content = dns_config(json.loads(data), upstream, rules)
    target = Path(target)
    temporary = target.with_suffix(".new")
    temporary.write_text(content)
    os.chmod(temporary, 0o644)
    # Stop the old resolver before flushing: an in-flight answer for a removed host must
    # not repopulate the broker sets between the flush and the configuration change.
    subprocess.run(["systemctl", "stop", "dnsmasq.service"], check=True)
    try:
        subprocess.run(["nft", "-f", "-"], input=(
            "flush set inet atomics_egress broker4\n"
            "flush set inet atomics_egress broker6\n"), text=True, check=True)
        temporary.replace(target)
    finally:
        # HUP does not reread nftset/server configuration. Starting also clears DNS caches.
        subprocess.run(["systemctl", "start", "dnsmasq.service"], check=True)


if __name__ == "__main__":
    try:
        reconcile(*sys.argv[1:])
    except Exception:
        sys.exit("secret broker network reconciliation failed (metadata hidden)")
