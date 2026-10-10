#!/usr/bin/env python3
"""Measure the network path to every builder and relay the bot submits to.

For each endpoint in `config/builders.toml` (and, for geography, the public
MEV-Boost relays below, which the bot does not use): the resolved
addresses, then per sample a fresh TCP connect, the TLS handshake on it, and
one cheap HTTPS request over that connection (`eth_chainId` for a builder
JSON-RPC, `GET /eth/v1/builder/status` for a relay). Each phase is timed on
its own, so connect ≈ one network round trip, TLS ≈ two more, request ≈ one
plus the server's work. Samples are sequential with a pause, so the numbers
are the path, not a burst.

Standard library only, so it runs unchanged on the production box:

    python tools/net/endpoint_latency.py [--samples 20] [--out ops/net/latency-<host>.json]

Prints a Markdown table (min / p50 / p95 per phase, ms) and writes the raw
samples as JSON beside it. Nothing here claims colocation: a p50 is a p50
from where the script ran, and the host name is in the output.
"""
from __future__ import annotations

import argparse
import json
import platform
import socket
import ssl
import statistics
import sys
import time
import tomllib
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[2]

# MEV-Boost relays (validator side). Not submission targets; measured so the
# box's distance to the relay set the proposers use is on record.
PUBLIC_RELAYS = {
    "relay:flashbots-boost": "https://boost-relay.flashbots.net",
    "relay:ultrasound": "https://relay.ultrasound.money",
    "relay:agnostic": "https://agnostic-relay.net",
    "relay:aestus": "https://mainnet.aestus.live",
    "relay:bloxroute-max-profit": "https://bloxroute.max-profit.blxrbdn.com",
    "relay:bloxroute-regulated": "https://bloxroute.regulated.blxrbdn.com",
    "relay:titan": "https://titanrelay.xyz",
}

RPC_BODY = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []}).encode()


def endpoints() -> list[tuple[str, str, str]]:
    """`(name, url, kind)`: the bot's builders from config, then the relays."""
    out = []
    cfg = tomllib.loads((ROOT / "config" / "builders.toml").read_text(encoding="utf-8"))
    for b in cfg.get("builders", []):
        out.append((f"builder:{b['name']}", b["endpoint"], "rpc"))
    share = cfg.get("mevshare", {}).get("relay")
    if share and all(u != share for _, u, _ in out):
        out.append(("mev-share:relay", share, "rpc"))
    for name, url in PUBLIC_RELAYS.items():
        out.append((name, url, "relay"))
    return out


def resolve(host: str) -> list[str]:
    try:
        infos = socket.getaddrinfo(host, 443, type=socket.SOCK_STREAM)
    except socket.gaierror:
        return []
    return sorted({i[4][0] for i in infos})


def one_sample(url: str, kind: str, timeout: float) -> dict:
    """Connect, handshake and one request on a fresh connection; ms per phase."""
    u = urlparse(url)
    host = u.hostname or ""
    port = u.port or 443
    path = u.path or "/"
    if kind == "relay":
        path = "/eth/v1/builder/status"
    ctx = ssl.create_default_context()
    t0 = time.perf_counter()
    sock = socket.create_connection((host, port), timeout=timeout)
    t1 = time.perf_counter()
    peer = sock.getpeername()[0]
    try:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        tls = ctx.wrap_socket(sock, server_hostname=host)
        t2 = time.perf_counter()
        try:
            if kind == "rpc":
                req = (
                    f"POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n"
                    f"Content-Length: {len(RPC_BODY)}\r\nConnection: close\r\n\r\n"
                ).encode() + RPC_BODY
            else:
                req = f"GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n".encode()
            tls.sendall(req)
            first = tls.recv(4096)
            t3 = time.perf_counter()
            status = first.split(b"\r\n", 1)[0].decode(errors="replace")
            # drain quietly so the server sees a clean close
            try:
                tls.settimeout(1.0)
                while tls.recv(4096):
                    pass
            except (OSError, ssl.SSLError):
                pass
        finally:
            tls.close()
    except Exception:
        sock.close()
        raise
    return {
        "connect_ms": (t1 - t0) * 1e3,
        "tls_ms": (t2 - t1) * 1e3,
        "request_ms": (t3 - t2) * 1e3,
        "peer": peer,
        "status": status,
    }


def pct(xs: list[float], p: float) -> float:
    if not xs:
        return float("nan")
    xs = sorted(xs)
    k = max(0, min(len(xs) - 1, round(p * (len(xs) - 1))))
    return xs[k]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--samples", type=int, default=20)
    ap.add_argument("--pause", type=float, default=0.25, help="seconds between samples")
    ap.add_argument("--timeout", type=float, default=5.0)
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()
    host = platform.node()
    when = datetime.now(timezone.utc).isoformat(timespec="seconds")
    results = []
    print(f"host {host}, {when}, {args.samples} samples per endpoint\n")
    print("| endpoint | addresses | connect min/p50/p95 | tls | request | ok |")
    print("|---|---|---|---|---|---|")
    for name, url, kind in endpoints():
        hostname = urlparse(url).hostname or ""
        addrs = resolve(hostname)
        samples, errors = [], []
        for _ in range(args.samples):
            try:
                samples.append(one_sample(url, kind, args.timeout))
            except Exception as e:  # noqa: BLE001 - recorded, not hidden
                errors.append(f"{type(e).__name__}: {e}"[:120])
            time.sleep(args.pause)
        row = {"name": name, "url": url, "kind": kind, "addresses": addrs, "samples": samples, "errors": errors}
        results.append(row)

        def col(key: str) -> str:
            xs = [s[key] for s in samples]
            if not xs:
                return "—"
            return f"{min(xs):.1f} / {statistics.median(xs):.1f} / {pct(xs, 0.95):.1f}"

        ok = f"{len(samples)}/{args.samples}"
        if samples:
            ok += f" ({samples[-1]['status'][:12]})"
        print(f"| {name} | {', '.join(addrs) or 'unresolved'} | {col('connect_ms')} | {col('tls_ms')} | {col('request_ms')} | {ok} |")
        sys.stdout.flush()
    out = args.out or (ROOT / "ops" / "net" / f"latency-{host}.json")
    out.write_text(json.dumps({"host": host, "when": when, "samples": args.samples, "results": results}, indent=1))
    print(f"\nraw samples: {out.relative_to(ROOT) if out.is_relative_to(ROOT) else out}")
    for r in results:
        if r["errors"]:
            print(f"  {r['name']}: {len(r['errors'])} failed — {r['errors'][0]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
