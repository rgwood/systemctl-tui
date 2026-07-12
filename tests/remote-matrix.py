#!/usr/bin/env -S uv run --script --quiet
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Runs the remote-mode integration tests against containers running real systemd
versions (239 -> 257), each with sshd and systemd-stdio-bridge installed.

Prefers `podman` if present, falls back to `docker`. Every container exposes sshd on
a random host port; a small `ssh` shim (prepended to PATH) transparently points both
the app under test and integration-test.py at the right port/key/known_hosts, so the
rest of the test harness doesn't need to know it's talking to a container.

Usage:
    ./tests/remote-matrix.py                          # run the whole matrix
    ./tests/remote-matrix.py --distro ubuntu-24.04     # just one distro
    ./tests/remote-matrix.py --keep                    # leave failed containers up for debugging
"""

import argparse
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MATRIX_DIR = REPO_ROOT / "tests" / "remote-matrix"
KEY_DIR = Path.home() / ".cache" / "sctui-remote-matrix"
KEY_PATH = KEY_DIR / "id_ed25519"

# distro name -> base image (approximate systemd version in the comment)
DISTROS = {
  "rocky-8": "rockylinux:8",  # systemd 239
  "ubuntu-20.04": "ubuntu:20.04",  # systemd 245
  "ubuntu-22.04": "ubuntu:22.04",  # systemd 249
  "debian-12": "debian:12",  # systemd 252
  "ubuntu-24.04": "ubuntu:24.04",  # systemd 255
  "fedora-latest": "fedora:latest",  # systemd ~257
}

# Hosts where remote mode CANNOT work; we assert it fails fast, before the TUI,
# with a clear error - never a hang or a panic. (Genuinely old systemd, pre-239,
# can't boot in containers on cgroup-v2 hosts, and pre-230 lacks the D-Bus API we
# use anyway - so "fails gracefully" is the only testable contract there.)
# name -> (what it is, substring the app's stderr must contain)
HOSTILE_HOSTS = {
  # alpine+sshd: no systemd at all -> the pre-flight bridge check should catch it
  "no-systemd": ("no systemd installed", "systemd-stdio-bridge not found"),
  # rocky-8 image booted with plain sshd instead of systemd: the bridge binary
  # exists (so the pre-flight check passes) but there is no bus to connect to
  "dead-systemd": ("systemd installed but not running", "Is systemd running on it?"),
}


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
  return subprocess.run(cmd, text=True, **kwargs)


def detect_engine() -> str:
  if shutil.which("podman"):
    return "podman"
  if shutil.which("docker"):
    return "docker"
  print("ERROR: neither podman nor docker found on PATH", file=sys.stderr)
  sys.exit(1)


def ensure_keypair() -> None:
  if KEY_PATH.exists():
    return
  KEY_DIR.mkdir(parents=True, exist_ok=True)
  print(f"generating persistent test keypair at {KEY_PATH}")
  result = run(["ssh-keygen", "-t", "ed25519", "-N", "", "-f", str(KEY_PATH)], capture_output=True)
  if result.returncode != 0:
    print(result.stdout, result.stderr, file=sys.stderr)
    sys.exit(1)


def container_name(distro: str) -> str:
  return f"sctui-matrix-{distro}"


def image_name(distro: str) -> str:
  return f"sctui-matrix-{distro}"


def build_image(engine: str, distro: str, base_image: str) -> bool:
  pubkey = (KEY_PATH.with_suffix(".pub")).read_text().strip()
  print(f"[{distro}] building image from {base_image}...")
  result = run(
    [
      engine,
      "build",
      "-f",
      str(MATRIX_DIR / "Containerfile"),
      "--build-arg",
      f"BASE_IMAGE={base_image}",
      "--build-arg",
      f"SSH_PUBKEY={pubkey}",
      "-t",
      image_name(distro),
      str(MATRIX_DIR),
    ],
  )
  return result.returncode == 0


def remove_stale_container(engine: str, distro: str) -> None:
  run([engine, "rm", "-f", container_name(distro)], capture_output=True)


def start_container(engine: str, distro: str, image: str | None = None, command: list[str] | None = None) -> bool:
  remove_stale_container(engine, distro)
  print(f"[{distro}] starting container...")
  result = run(
    [
      engine,
      "run",
      "-d",
      "--privileged",
      "--name",
      container_name(distro),
      "-p",
      "127.0.0.1::22",
      image or image_name(distro),
      *(command or []),
    ],
  )
  return result.returncode == 0


def get_mapped_port(engine: str, distro: str) -> int | None:
  result = run([engine, "port", container_name(distro), "22"], capture_output=True)
  if result.returncode != 0:
    return None
  # output like "0.0.0.0:34567" or "127.0.0.1:34567"
  line = result.stdout.strip().splitlines()[0]
  port = line.rsplit(":", 1)[-1]
  try:
    return int(port)
  except ValueError:
    return None


def make_ssh_shim(shim_dir: Path, port: int, key_path: Path, known_hosts: Path) -> None:
  shim_path = shim_dir / "ssh"
  shim_path.write_text(
    "#!/bin/sh\n"
    f'exec /usr/bin/ssh -p {port} -i {key_path} -o UserKnownHostsFile={known_hosts} '
    '-o StrictHostKeyChecking=accept-new -o IdentitiesOnly=yes "$@"\n'
  )
  shim_path.chmod(shim_path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


def wait_for_ready(shim_env: dict, timeout: float = 90.0) -> bool:
  deadline = time.monotonic() + timeout
  ok_states = {"running", "degraded"}
  while time.monotonic() < deadline:
    root_result = run(
      ["ssh", "root@127.0.0.1", "systemctl", "is-system-running"],
      capture_output=True,
      env=shim_env,
      timeout=10,
    )
    root_state = root_result.stdout.strip()
    if root_state in ok_states:
      user_result = run(
        ["ssh", "testuser@127.0.0.1", "systemctl", "--user", "is-system-running"],
        capture_output=True,
        env=shim_env,
        timeout=10,
      )
      user_state = user_result.stdout.strip()
      if user_state in ok_states:
        return True
    time.sleep(2)
  return False


def run_distro(engine: str, distro: str, base_image: str, binary: str, keep: bool) -> bool:
  print(f"\n{'=' * 60}\n{distro} ({base_image})\n{'=' * 60}")

  if not build_image(engine, distro, base_image):
    print(f"[{distro}] FAIL: image build failed")
    return False

  if not start_container(engine, distro):
    print(f"[{distro}] FAIL: container failed to start")
    return False

  success = False
  try:
    port = None
    for _ in range(10):
      port = get_mapped_port(engine, distro)
      if port:
        break
      time.sleep(0.5)
    if not port:
      print(f"[{distro}] FAIL: could not determine mapped ssh port")
      return False

    with tempfile.TemporaryDirectory(prefix=f"sctui-matrix-{distro}-") as tmpdir:
      tmp_path = Path(tmpdir)
      known_hosts = tmp_path / "known_hosts"
      known_hosts.touch()
      make_ssh_shim(tmp_path, port, KEY_PATH, known_hosts)

      shim_env = dict(os.environ)
      shim_env["PATH"] = f"{tmp_path}{os.pathsep}{shim_env.get('PATH', '')}"

      print(f"[{distro}] waiting for system + user managers to come up (port {port})...")
      if not wait_for_ready(shim_env):
        print(f"[{distro}] FAIL: system/user manager did not reach running/degraded in time")
        return False

      print(f"[{distro}] running integration tests...")
      test_result = run(
        [
          str(REPO_ROOT / "tests" / "integration-test.py"),
          "--host",
          "testuser@127.0.0.1",
          "--remote-suite",
          "--binary",
          binary,
        ],
        env=shim_env,
        cwd=str(REPO_ROOT),
      )
      success = test_result.returncode == 0
      if not success:
        print(f"[{distro}] FAIL: integration tests failed (exit {test_result.returncode})")
      else:
        print(f"[{distro}] PASS")
      return success
  finally:
    if success or not keep:
      run([engine, "rm", "-f", container_name(distro)], capture_output=True)
    else:
      print(f"[{distro}] --keep set and run failed: leaving container '{container_name(distro)}' running for debugging")


def wait_for_ssh(shim_env: dict, timeout: float = 60.0) -> bool:
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if run(["ssh", "root@127.0.0.1", "true"], capture_output=True, env=shim_env, timeout=10).returncode == 0:
      return True
    time.sleep(1)
  return False


def run_hostile(engine: str, name: str, binary: str, keep: bool) -> bool:
  description, expected_error = HOSTILE_HOSTS[name]
  print(f"\n{'=' * 60}\n{name} ({description})\n{'=' * 60}")

  if name == "no-systemd":
    pubkey = KEY_PATH.with_suffix(".pub").read_text().strip()
    result = run(
      [
        engine,
        "build",
        "-f",
        str(MATRIX_DIR / "Containerfile.no-systemd"),
        "--build-arg",
        f"SSH_PUBKEY={pubkey}",
        "-t",
        image_name(name),
        str(MATRIX_DIR),
      ],
    )
    if result.returncode != 0:
      print(f"[{name}] FAIL: image build failed")
      return False
    started = start_container(engine, name)
  else:
    # dead-systemd: reuse the rocky-8 image, but boot plain sshd instead of systemd
    if not build_image(engine, "rocky-8", DISTROS["rocky-8"]):
      print(f"[{name}] FAIL: image build failed")
      return False
    started = start_container(engine, name, image=image_name("rocky-8"), command=["/usr/sbin/sshd", "-D", "-e"])

  if not started:
    print(f"[{name}] FAIL: container failed to start")
    return False

  success = False
  try:
    port = None
    for _ in range(10):
      port = get_mapped_port(engine, name)
      if port:
        break
      time.sleep(0.5)
    if not port:
      print(f"[{name}] FAIL: could not determine mapped ssh port")
      return False

    with tempfile.TemporaryDirectory(prefix=f"sctui-matrix-{name}-") as tmpdir:
      tmp_path = Path(tmpdir)
      known_hosts = tmp_path / "known_hosts"
      known_hosts.touch()
      make_ssh_shim(tmp_path, port, KEY_PATH, known_hosts)

      shim_env = dict(os.environ)
      shim_env["PATH"] = f"{tmp_path}{os.pathsep}{shim_env.get('PATH', '')}"

      print(f"[{name}] waiting for sshd (port {port})...")
      if not wait_for_ssh(shim_env):
        print(f"[{name}] FAIL: sshd did not come up in time")
        return False

      # The contract: fail fast (no hang), fail loud (nonzero + clear message),
      # and fail before the TUI (stderr is a plain error, not escape codes).
      print(f"[{name}] running the app, expecting a clean failure...")
      try:
        result = subprocess.run(
          [binary, "--host", "root@127.0.0.1"],
          capture_output=True,
          text=True,
          timeout=30,
          env=shim_env,
        )
      except subprocess.TimeoutExpired:
        print(f"[{name}] FAIL: app hung for 30s instead of failing fast")
        return False

      problems = []
      if result.returncode == 0:
        problems.append(f"expected nonzero exit, got 0")
      if expected_error not in result.stderr:
        problems.append(f"stderr missing {expected_error!r}:\n{result.stderr.strip()}")
      if "\x1b[?1049h" in result.stderr:
        problems.append("app entered the alternate screen (TUI) before failing")

      if problems:
        print(f"[{name}] FAIL:")
        for p in problems:
          print(f"  {p}")
      else:
        lines = result.stderr.strip().splitlines()
        # the app prints a backtrace after the error (it sets RUST_BACKTRACE=1 itself),
        # so show the actual "Error:" line rather than whatever comes last
        error_line = next((line for line in lines if line.startswith("Error:")), lines[-1] if lines else "")
        print(f"[{name}] PASS (exit {result.returncode}: {error_line})")
        success = True
      return success
  finally:
    if success or not keep:
      run([engine, "rm", "-f", container_name(name)], capture_output=True)
    else:
      print(f"[{name}] --keep set and run failed: leaving container '{container_name(name)}' running for debugging")


def main() -> int:
  parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
  parser.add_argument(
    "--distro",
    action="append",
    choices=sorted(DISTROS) + sorted(HOSTILE_HOSTS),
    help="distro to test (repeatable; default: all, including the hostile hosts)",
  )
  parser.add_argument("--engine", choices=["podman", "docker"], default=None, help="container engine (default: auto-detect)")
  parser.add_argument("--keep", action="store_true", help="leave a container running on failure, for debugging")
  parser.add_argument("--binary", default="./target/debug/systemctl-tui")
  args = parser.parse_args()

  engine = args.engine or detect_engine()
  distros = args.distro or sorted(DISTROS) + sorted(HOSTILE_HOSTS)

  if not Path(args.binary).exists():
    print(f"ERROR: binary not found at {args.binary} (run `cargo build` first)", file=sys.stderr)
    return 2

  ensure_keypair()

  results: dict[str, bool] = {}
  for distro in distros:
    if distro in HOSTILE_HOSTS:
      results[distro] = run_hostile(engine, distro, args.binary, args.keep)
    else:
      results[distro] = run_distro(engine, distro, DISTROS[distro], args.binary, args.keep)

  print(f"\n{'=' * 60}\nsummary\n{'=' * 60}")
  for distro, ok in results.items():
    print(f"  {'PASS' if ok else 'FAIL'}  {distro}")

  return 0 if all(results.values()) else 1


if __name__ == "__main__":
  sys.exit(main())
