#!/usr/bin/env python3
"""Isolated Manarouter smoke against mock-upstream with a disposable synthetic credential."""

from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


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


def wait_http(url: str, process: subprocess.Popen[bytes], log_path: Path, timeout: float = 15.0) -> bytes:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            log_text = log_path.read_text(errors="replace") if log_path.exists() else ""
            raise RuntimeError(f"process exited with {process.returncode}; logs:\n{log_text}")
        try:
            with urllib.request.urlopen(url, timeout=0.5) as response:
                return response.read()
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            last_error = exc
            time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for {url}: {last_error}; see {log_path}")


def request_json(url: str, payload: dict | None = None) -> tuple[int, dict]:
    data = None if payload is None else json.dumps(payload).encode()
    request = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json"} if data is not None else {},
        method="POST" if data is not None else "GET",
    )
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as exc:
        raw = exc.read()
        try:
            payload = json.loads(raw)
        except (json.JSONDecodeError, UnicodeDecodeError):
            payload = {"raw_body": raw.decode(errors="replace")}
        return exc.code, payload


def build(command: list[str], cwd: Path, env: dict[str, str]) -> None:
    print("+", " ".join(command), flush=True)
    subprocess.run(command, cwd=cwd, env=env, check=True)


def main() -> int:
    cargo = os.environ.get("CARGO", "cargo")
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(ROOT / "target")
    build([cargo, "build", "--locked", "-p", "mr-cli", "-p", "manarouter-conductor", "-p", "manarouter-relay"], ROOT, env)
    mock_manifest = ROOT / "tools/mock-upstream/Cargo.toml"
    build([cargo, "build", "--locked", "--manifest-path", str(mock_manifest), "--target-dir", str(ROOT / "target/mock-upstream"), "--bin", "mock-upstream"], ROOT, env)

    conductor_bin = ROOT / "target/debug/manarouter-conductor"
    relay_bin = ROOT / "target/debug/manarouter-relay"
    mock_bin = ROOT / "target/mock-upstream/debug/mock-upstream"
    for binary in (conductor_bin, relay_bin, mock_bin):
        if not binary.is_file():
            raise RuntimeError(f"build did not produce {binary}")

    with tempfile.TemporaryDirectory(prefix="manarouter-e2e-") as temp_name:
        temp = Path(temp_name)
        config_dir = temp / "config"
        state_dir = temp / "state"
        config_dir.mkdir(mode=0o700)
        state_dir.mkdir(mode=0o700)
        (mock_port, relay_port, control_port), port_sockets = reserve_ports(3)
        # Local-only provider starts without an account; the CLI below creates one with a synthetic
        # test-only value that is encrypted into this temporary config directory's vault.
        config = {
            "relay": {"host": "127.0.0.1", "port": relay_port, "reuse_port": False, "backlog": 64},
            "control": {"host": "127.0.0.1", "port": control_port, "allow_remote_without_password": False, "password_hash": None},
            "providers": [{
                "id": "mock",
                "dialect": "open-ai",
                "base_url": f"http://127.0.0.1:{mock_port}",
                "auth": {"type": "bearer"},
                "accounts": [],
                "models": [{"id": "mock-model"}],
                "enabled": True,
                "connect_timeout_ms": 1000,
                "ttfb_timeout_ms": 3000,
                "idle_timeout_ms": 5000,
            }],
            "combos": [], "aliases": [], "client_keys": [],
        }
        (config_dir / "config.json").write_text(json.dumps(config), encoding="utf-8")
        os.chmod(config_dir / "config.json", 0o600)

        proc_env = env.copy()
        # These explicit paths ensure this test cannot read/write a developer's usual directories.
        proc_env["MANAROUTER_CONFIG_DIR"] = str(config_dir)
        proc_env["MANAROUTER_STATE_DIR"] = str(state_dir)
        proc_env["HOME"] = str(temp)
        proc_env["XDG_CONFIG_HOME"] = str(temp / "xdg-config")
        proc_env["XDG_STATE_HOME"] = str(temp / "xdg-state")
        proc_env["RUST_LOG"] = "warn"

        procs: list[tuple[str, subprocess.Popen[bytes], object]] = []
        try:
            # This fake value is not a usable provider token. Feed through the normal CLI so the
            # vault key and encrypted vault are created only under config_dir.
            subprocess.run(
                [str(ROOT / "target/debug/mr"), "--config-dir", str(config_dir), "account", "add", "mock", "--label", "e2e", "--secret-stdin"],
                cwd=ROOT, env=proc_env, input=b"e2e-placeholder-not-a-real-token\n", check=True,
                stdout=subprocess.DEVNULL,
            )
            config_after = json.loads((config_dir / "config.json").read_text(encoding="utf-8"))
            account = config_after["providers"][0]["accounts"][0]
            if account.get("secret_ref") != "provider/mock/account/" + account["id"]:
                raise AssertionError(f"CLI did not add expected vault reference: {account}")
            if "e2e-placeholder-not-a-real-token" in (config_dir / "config.json").read_text(encoding="utf-8"):
                raise AssertionError("synthetic credential leaked into config.json")

            # Keep all chosen ports reserved while preparing the fixture, then release them
            # together immediately before launching the three local listeners.
            for port_socket in port_sockets:
                port_socket.close()
            mock_log = (temp / "mock.log").open("wb")
            mock = subprocess.Popen(
                [str(mock_bin), "--bind", "127.0.0.1", "--port", str(mock_port)],
                cwd=ROOT, env=proc_env, stdout=mock_log, stderr=subprocess.STDOUT,
            )
            procs.append(("mock-upstream", mock, mock_log))
            wait_http(f"http://127.0.0.1:{mock_port}/healthz", mock, temp / "mock.log")

            conductor_log = (temp / "conductor.log").open("wb")
            conductor = subprocess.Popen(
                [str(conductor_bin)], cwd=ROOT, env=proc_env,
                stdout=conductor_log, stderr=subprocess.STDOUT,
            )
            procs.append(("conductor", conductor, conductor_log))
            health = json.loads(wait_http(
                f"http://127.0.0.1:{control_port}/api/health", conductor, temp / "conductor.log"
            ))
            if health.get("counts", {}).get("providers") != 1:
                raise AssertionError(f"conductor loaded unexpected isolated config: {health}")

            relay_log = (temp / "relay.log").open("wb")
            relay = subprocess.Popen(
                [str(relay_bin), "--host", "127.0.0.1", "--port", str(relay_port), "--config-dir", str(config_dir)],
                cwd=ROOT, env=proc_env, stdout=relay_log, stderr=subprocess.STDOUT,
            )
            procs.append(("relay", relay, relay_log))
            models = json.loads(wait_http(
                f"http://127.0.0.1:{relay_port}/v1/models", relay, temp / "relay.log"
            ))
            if not any(row.get("id") == "mock/mock-model" for row in models.get("data", [])):
                raise AssertionError(f"relay did not publish expected model: {models}")

            status, result = request_json(
                f"http://127.0.0.1:{relay_port}/v1/chat/completions",
                {"model": "mock/mock-model", "messages": [{"role": "user", "content": "smoke check"}]},
            )
            if status != 200:
                raise AssertionError(f"relay returned HTTP {status}: {result}")
            # The relay forwards the provider's response model identifier; it is allowed to be
            # upstream-specific (mock-model) rather than echoing the client-facing qualified name.
            message = result.get("choices", [{}])[0].get("message", {})
            if message.get("content") != "Hello from mock-upstream.":
                raise AssertionError(f"unexpected mock response: {result}")
            if result.get("echo", {}).get("model") != "mock-model":
                raise AssertionError(f"mock upstream model mismatch: {result}")
            # Verify the synthetic secret is present only in the encrypted vault and known fixture
            # files remain inside the disposable config/state roots. The caller key is omitted.
            if not (config_dir / "vault.key").is_file() or not (config_dir / "vault.enc").is_file():
                raise AssertionError("expected only the isolated encrypted fixture vault")
            vault_text = (config_dir / "vault.enc").read_text(encoding="utf-8")
            if "e2e-placeholder-not-a-real-token" in vault_text:
                raise AssertionError("synthetic credential was not encrypted in vault.enc")
            print("PASS: isolated conductor + relay + mock-upstream; local chat request routed successfully")
            print(f"PASS: isolated config/state under {temp}; only a synthetic provider value, no real credentials or external endpoint")
            return 0
        finally:
            for name, proc, log_file in reversed(procs):
                if proc.poll() is None:
                    proc.send_signal(signal.SIGTERM)
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=5)
                log_file.close()
                if proc.returncode not in (0, -signal.SIGTERM):
                    print(f"{name} exited with {proc.returncode}; output was in temporary directory", file=sys.stderr)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        raise
