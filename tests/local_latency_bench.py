#!/usr/bin/env python3
"""Report-only paired local benchmark: direct mock vs relay-routed mock.

All listeners bind to 127.0.0.1. Runtime config, state, HOME, and synthetic credential
live under a TemporaryDirectory. No real provider endpoints or credentials are used.
"""
from __future__ import annotations

import argparse
import http.client
import json
import os
from pathlib import Path
import platform
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
SYNTHETIC_SECRET = "local-benchmark-placeholder-not-a-real-token"
MAX_WARMUPS = 1000
MAX_SAMPLES = 10000


def bounded_int(value: str, *, minimum: int, maximum: int, name: str) -> int:
    try:
        number = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"{name} must be an integer") from exc
    if not minimum <= number <= maximum:
        raise argparse.ArgumentTypeError(f"{name} must be between {minimum} and {maximum}")
    return number


def reserve_ports(count: int) -> tuple[list[int], list[socket.socket]]:
    sockets: list[socket.socket] = []
    ports: list[int] = []
    try:
        for _ in range(count):
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.bind(("127.0.0.1", 0))
            sockets.append(sock)
            ports.append(sock.getsockname()[1])
        return ports, sockets
    except BaseException:
        for sock in sockets:
            sock.close()
        raise


def build(command: list[str], env: dict[str, str]) -> None:
    print("+", " ".join(command), flush=True)
    subprocess.run(command, cwd=ROOT, env=env, check=True)


# Health checks must not follow `http_proxy`/`HTTP_PROXY` from the environment: every listener here is
# loopback, and a configured proxy would turn a healthy local process into a startup timeout.
_LOOPBACK_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def wait_http(url: str, process: subprocess.Popen[bytes], log_path: Path, timeout: float = 20) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"process exited {process.returncode}: {log_path.read_text(errors='replace')}")
        try:
            with _LOOPBACK_OPENER.open(url, timeout=0.5) as response:
                response.read()
                return
        except (urllib.error.URLError, http.client.HTTPException, TimeoutError, OSError) as exc:
            last_error = exc
            time.sleep(0.05)
    raise RuntimeError(f"timed out waiting for {url}: {last_error}; see {log_path}")


def stop_children(children: list[tuple[subprocess.Popen[bytes], object]]) -> None:
    for process, log_file in reversed(children):
        try:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        pass
        finally:
            log_file.close()


def request(connection: http.client.HTTPConnection, path: str, body: dict) -> tuple[float, str | None]:
    encoded = json.dumps(body, separators=(",", ":")).encode()
    started = time.perf_counter_ns()
    try:
        connection.request("POST", path, body=encoded, headers={"Content-Type": "application/json"})
        response = connection.getresponse()
        raw = response.read()  # consume the complete body so HTTPConnection can reuse TCP connection
        elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
        if response.status != 200:
            return elapsed_ms, f"HTTP {response.status}: {raw[:300].decode(errors='replace')}"
        try:
            payload = json.loads(raw)
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            return elapsed_ms, f"invalid JSON response: {exc}"
        if not isinstance(payload, dict):
            return elapsed_ms, "response was not a JSON object"
        choices = payload.get("choices")
        if not isinstance(choices, list) or not choices:
            return elapsed_ms, "response did not contain choices"
        return elapsed_ms, None
    except (OSError, http.client.HTTPException, TimeoutError) as exc:
        return (time.perf_counter_ns() - started) / 1_000_000, f"{type(exc).__name__}: {exc}"


def distribution(values: list[float]) -> dict[str, float | int | None]:
    if not values:
        return {"count": 0, "min_ms": None, "p50_ms": None, "p90_ms": None, "p95_ms": None, "p99_ms": None, "max_ms": None, "mean_ms": None, "stdev_ms": None}
    ordered = sorted(values)

    def percentile(p: float) -> float:
        # Nearest-rank method, documented in report metadata.
        return ordered[max(0, min(len(ordered) - 1, int((p * len(ordered) + 0.999999) // 1) - 1))]

    return {
        "count": len(values),
        "min_ms": ordered[0],
        "p50_ms": percentile(0.50),
        "p90_ms": percentile(0.90),
        "p95_ms": percentile(0.95),
        "p99_ms": percentile(0.99),
        "max_ms": ordered[-1],
        "mean_ms": statistics.fmean(values),
        "stdev_ms": statistics.stdev(values) if len(values) > 1 else 0.0,
    }


def run_benchmark(warmups: int, samples: int, timeout: float) -> dict:
    env = os.environ.copy()
    cargo = env.get("CARGO", "cargo")
    target_dir = ROOT / "target"
    env["CARGO_TARGET_DIR"] = str(target_dir)
    build([cargo, "build", "--locked", "--release", "-p", "mr-cli", "-p", "manarouter-conductor", "-p", "manarouter-relay"], env)
    build([cargo, "build", "--locked", "--release", "--manifest-path", str(ROOT / "tools/mock-upstream/Cargo.toml"), "--target-dir", str(target_dir / "mock-upstream"), "--bin", "mock-upstream"], env)

    cli = target_dir / "release/mr"
    conductor_bin = target_dir / "release/manarouter-conductor"
    relay_bin = target_dir / "release/manarouter-relay"
    mock_bin = target_dir / "mock-upstream/release/mock-upstream"
    for binary in (cli, conductor_bin, relay_bin, mock_bin):
        if not binary.is_file():
            raise RuntimeError(f"release build did not produce {binary}")

    with tempfile.TemporaryDirectory(prefix="manarouter-local-bench-") as temp_name:
        temp = Path(temp_name)
        config_dir, state_dir = temp / "config", temp / "state"
        config_dir.mkdir(mode=0o700)
        state_dir.mkdir(mode=0o700)
        (mock_port, relay_port, control_port), reserved = reserve_ports(3)
        config = {
            "relay": {"host": "127.0.0.1", "port": relay_port, "reuse_port": False, "backlog": 64},
            "control": {"host": "127.0.0.1", "port": control_port, "allow_remote_without_password": False, "password_hash": None},
            "providers": [{
                "id": "bench-mock", "dialect": "open-ai", "base_url": f"http://127.0.0.1:{mock_port}",
                "auth": {"type": "bearer"}, "accounts": [], "models": [{"id": "bench-model"}],
                "enabled": True, "connect_timeout_ms": 1000, "ttfb_timeout_ms": 5000, "idle_timeout_ms": 5000,
            }],
            "combos": [], "aliases": [], "client_keys": [],
        }
        config_path = config_dir / "config.json"
        config_path.write_text(json.dumps(config), encoding="utf-8")
        config_path.chmod(0o600)
        child_env = env.copy()
        child_env.update({
            "MANAROUTER_CONFIG_DIR": str(config_dir), "MANAROUTER_STATE_DIR": str(state_dir),
            "HOME": str(temp), "XDG_CONFIG_HOME": str(temp / "xdg-config"),
            "XDG_STATE_HOME": str(temp / "xdg-state"), "RUST_LOG": "error",
        })
        children: list[tuple[subprocess.Popen[bytes], object]] = []
        errors: list[dict[str, str | int]] = []
        try:
            subprocess.run(
                [str(cli), "--config-dir", str(config_dir), "account", "add", "bench-mock", "--label", "local-benchmark", "--secret-stdin"],
                cwd=ROOT, env=child_env, input=(SYNTHETIC_SECRET + "\n").encode(), check=True,
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=15,
            )
            for sock in reserved:
                sock.close()

            def spawn(name: str, argv: list[str]) -> subprocess.Popen[bytes]:
                log_file = (temp / f"{name}.log").open("wb")
                process = subprocess.Popen(argv, cwd=ROOT, env=child_env, stdout=log_file, stderr=subprocess.STDOUT)
                children.append((process, log_file))
                return process

            mock = spawn("mock", [str(mock_bin), "--bind", "127.0.0.1", "--port", str(mock_port)])
            wait_http(f"http://127.0.0.1:{mock_port}/healthz", mock, temp / "mock.log")
            conductor = spawn("conductor", [str(conductor_bin)])
            wait_http(f"http://127.0.0.1:{control_port}/api/health", conductor, temp / "conductor.log")
            relay = spawn("relay", [str(relay_bin), "--host", "127.0.0.1", "--port", str(relay_port), "--config-dir", str(config_dir)])
            wait_http(f"http://127.0.0.1:{relay_port}/v1/models", relay, temp / "relay.log")

            direct = http.client.HTTPConnection("127.0.0.1", mock_port, timeout=timeout)
            routed = http.client.HTTPConnection("127.0.0.1", relay_port, timeout=timeout)
            observations: dict[str, list[float]] = {"direct": [], "relay": []}
            paired: list[float] = []
            warmup_errors = 0
            total = warmups + samples
            for index in range(total):
                direct_model, relay_model = "bench-model", "bench-mock/bench-model"
                payload_direct = {"model": direct_model, "messages": [{"role": "user", "content": "local latency sample"}]}
                payload_routed = {"model": relay_model, "messages": [{"role": "user", "content": "local latency sample"}]}
                # Alternate order to reduce systematic bias while preserving a direct/routed pair.
                order = ("direct", "relay") if index % 2 == 0 else ("relay", "direct")
                pair_times: dict[str, float] = {}
                pair_failed = False
                for mode in order:
                    conn, model, path = (direct, direct_model, "/v1/chat/completions") if mode == "direct" else (routed, relay_model, "/v1/chat/completions")
                    payload = payload_direct if mode == "direct" else payload_routed
                    elapsed, error = request(conn, path, payload)
                    if error:
                        pair_failed = True
                        errors.append({"sample_index": index, "path": mode, "error": error})
                    pair_times[mode] = elapsed
                if index < warmups:
                    if pair_failed:
                        warmup_errors += 1
                    continue
                if not pair_failed:
                    observations["direct"].append(pair_times["direct"])
                    observations["relay"].append(pair_times["relay"])
                    paired.append(pair_times["relay"] - pair_times["direct"])
            direct.close()
            routed.close()
            return {
                "schema_version": 1,
                "kind": "report-only-local-latency-benchmark",
                "created_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "machine": {"hostname": platform.node(), "os": platform.platform(), "system": platform.system(), "release": platform.release(), "architecture": platform.machine(), "python": platform.python_version(), "cpu_count": os.cpu_count()},
                "profile": "release",
                "topology": {
                    "direct": "persistent HTTP connection to loopback mock-upstream",
                    "routed": "persistent HTTP connection through loopback Manarouter relay to the same loopback mock-upstream",
                    "services": ["mock-upstream", "manarouter-conductor", "manarouter-relay"],
                    "network": "loopback only; no external endpoints",
                    "config_state_home": "temporary isolated directories",
                    "credential": "synthetic placeholder entered with mr account add --secret-stdin and stored only in temporary encrypted vault",
                    "paired_order": "alternating direct-first and relay-first within each sequential pair",
                },
                "settings": {"warmups_per_path": warmups, "samples_per_path": samples, "timeout_seconds": timeout, "percentile_method": "nearest-rank", "threshold": None},
                "results": {
                    "direct_ms": distribution(observations["direct"]),
                    "relay_routed_ms": distribution(observations["relay"]),
                    "paired_delta_relay_minus_direct_ms": distribution(paired),
                    "successful_pairs": len(paired),
                    "requested_samples": samples,
                    "warmup_pairs_with_errors": warmup_errors,
                },
                "errors": errors,
            }
        finally:
            for sock in reserved:
                sock.close()
            stop_children(children)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--warmups", type=lambda s: bounded_int(s, minimum=0, maximum=MAX_WARMUPS, name="warmups"), default=10)
    parser.add_argument("--samples", type=lambda s: bounded_int(s, minimum=1, maximum=MAX_SAMPLES, name="samples"), default=100)
    parser.add_argument("--timeout", type=float, default=10.0, help="per-request timeout in seconds (0.1 to 120)")
    parser.add_argument("--output", type=Path, help="write JSON report to this path; default prints to stdout")
    args = parser.parse_args()
    if not 0.1 <= args.timeout <= 120:
        parser.error("timeout must be between 0.1 and 120 seconds")
    try:
        report = run_benchmark(args.warmups, args.samples, args.timeout)
    except (OSError, subprocess.SubprocessError, RuntimeError) as exc:
        print(f"benchmark setup failed: {exc}", file=sys.stderr)
        return 2
    text = json.dumps(report, indent=2, sort_keys=True)
    if args.output:
        args.output.write_text(text + "\n", encoding="utf-8")
    else:
        print(text)
    return 1 if report["errors"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
