#!/usr/bin/env python3
# python3 ondemand-kmod/tests/test-ondemand-procfs.py --arch riscv64 --port 4444

import argparse
import os
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path


def wait_for_prompt(sock: socket.socket, timeout: float = 90.0):
    prompt = "starry:~#"
    data = ""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            chunk = sock.recv(4096).decode("utf-8", errors="ignore")
        except socket.timeout:
            continue
        if not chunk:
            break
        print(chunk, end="")
        data += chunk
        if prompt in data:
            return data
    raise RuntimeError("Timed out waiting for shell prompt")


def drain_socket(sock: socket.socket, max_wait: float = 0.3):
    deadline = time.time() + max_wait
    while time.time() < deadline:
        try:
            chunk = sock.recv(4096)
        except socket.timeout:
            return
        if not chunk:
            return


def run_cmd(
    sock: socket.socket,
    cmd: str,
    timeout: float = 20.0,
    expect_prompt: bool = True,
) -> str:
    prompt = "starry:~#"
    drain_socket(sock)
    sock.sendall((cmd + "\r\n").encode())

    output = ""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            chunk = sock.recv(4096).decode("utf-8", errors="ignore")
        except socket.timeout:
            continue
        if not chunk:
            # For commands like "exit", shell may close the connection
            # instead of printing the next prompt.
            if not expect_prompt:
                return output
            break
        print(chunk, end="")
        output += chunk
        if expect_prompt and prompt in output:
            return output
    if not expect_prompt:
        return output
    raise RuntimeError(f"Timed out running command: {cmd}")


def wait_for_tcp_server(host: str, port: int, timeout: float, proc: subprocess.Popen):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError("QEMU exited early")
        try:
            s = socket.create_connection((host, port), timeout=1.0)
            s.settimeout(0.5)
            return s
        except OSError:
            time.sleep(0.2)
    raise RuntimeError("QEMU did not start in time")


def resolve_starry_root(user_input: str | None) -> Path:
    if user_input:
        root = Path(user_input).expanduser().resolve()
    else:
        env_root = os.environ.get("STARRYOS_ROOT")
        if env_root:
            root = Path(env_root).expanduser().resolve()
        else:
            # Fallback for monorepo layout: <root>/ondemand-kmod/tests/...
            root = Path(__file__).resolve().parents[2]

    cargo = root / "Cargo.toml"
    arceos = root / "arceos"
    if not cargo.exists() or not arceos.is_dir():
        raise RuntimeError(
            f"Invalid StarryOS root: {root}. "
            "Pass --starry-root or set STARRYOS_ROOT."
        )
    return root


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--arch", default="riscv64")
    parser.add_argument("--port", default="4444")
    parser.add_argument(
        "--starry-root",
        default=None,
        help="Path to StarryOS repository root (or set STARRYOS_ROOT)",
    )
    args = parser.parse_args()
    starry_root = resolve_starry_root(args.starry_root)

    qemu = subprocess.Popen(
        [
            "make",
            f"ARCH={args.arch}",
            "ACCEL=n",
            "justrun",
            f"QEMU_ARGS=-monitor none -serial tcp::{args.port},server=on",
        ],
        cwd=starry_root,
        stderr=subprocess.PIPE,
        text=True,
    )

    def worker():
        for line in qemu.stderr:
            print(line, file=sys.stderr, end="")

    t = threading.Thread(target=worker, daemon=True)
    t.start()

    sock = None
    try:
        sock = wait_for_tcp_server("localhost", int(args.port), timeout=45.0, proc=qemu)
        wait_for_prompt(sock)

        # 1) Before first /proc access, procfs should not be loaded.
        run_cmd(sock, "dmesg | grep \"\\[ondemand\\] loading module 'procfs'\" >/dev/null")

        # 2) Trigger first access.
        out = run_cmd(sock, "cat /proc/meminfo >/dev/null; echo RC:$?")
        if "RC:0" not in out:
            raise RuntimeError("cat /proc/meminfo failed, procfs did not load")

        # 3) Verify load log exists.
        out = run_cmd(sock, "dmesg | grep \"\\[ondemand\\] loading module 'procfs'\"")
        if "loading module 'procfs'" not in out:
            raise RuntimeError("No procfs loading log found")

        # 4) Wait and verify unload happened.
        out = run_cmd(sock, "sleep 7; dmesg | grep \"\\[ondemand\\] unload handle\"")
        if "unload handle" not in out:
            raise RuntimeError("No unload log found (idle unload may not be working)")

        print("\n\x1b[32m✔ On-demand procfs load/unload test passed\x1b[0m")
        run_cmd(sock, "exit", expect_prompt=False)
    finally:
        if sock is not None:
            try:
                sock.close()
            except Exception:
                pass
        try:
            qemu.wait(2)
        except subprocess.TimeoutExpired:
            qemu.terminate()
            qemu.wait()


if __name__ == "__main__":
    main()
