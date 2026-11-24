#!/usr/bin/env python3
"""Private Docker network fixture for peer-port tests; no image pulls or host ports."""
import argparse
import asyncio
import contextlib
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
from urllib.parse import urlsplit


LABEL = "ssmcp.peer-netns"


def owned_name(value):
    if not re.fullmatch(r"ssmcp-peer-test-[0-9a-f]{32}", value):
        raise argparse.ArgumentTypeError("expected the unique peer fixture directory name")
    return value


def endpoint(value):
    host, port = value.rsplit(":", 1)
    if host != "127.0.0.1" or not 1 <= int(port) <= 65535:
        raise ValueError("fixture endpoints must be nonzero IPv4 loopback ports")
    return host, int(port)


def docker(*args, timeout=5):
    return subprocess.run(
        ["docker", *args], check=True, capture_output=True, text=True, timeout=timeout
    ).stdout.strip()


def cleanup(args):
    # Labels ensure cleanup owns each resource; races with Docker --rm are harmless.
    errors = []

    def absent(error):
        if not isinstance(error, subprocess.CalledProcessError):
            return False
        message = error.stderr.lower()
        return "no such object" in message or ("network" in message and "not found" in message)

    def remove_owned(inspect, remove):
        try:
            label = docker(*inspect)
        except (subprocess.SubprocessError, OSError) as error:
            if not absent(error):
                errors.append(str(error))
            return
        if label != args.network:
            errors.append(f"refusing resource without this fixture's ownership label: {inspect[-1]}")
            return
        try:
            docker(*remove)
        except (subprocess.SubprocessError, OSError) as error:
            if not absent(error):
                errors.append(str(error))

    for role in ("listen", "target"):
        name = f"{args.network}-{role}"
        remove_owned(
            ("inspect", "--format", f'{{{{ index .Config.Labels "{LABEL}" }}}}', name),
            ("rm", "-f", name),
        )
    for network in (args.network, f"{args.network}-listen-home"):
        remove_owned(
            ("network", "inspect", "--format", f'{{{{ index .Labels "{LABEL}" }}}}', network),
            ("network", "rm", network),
        )
    for error in errors:
        print(f"peer fixture cleanup: {error}", file=sys.stderr)
    return 1 if errors else 0


def launch(args):
    root = Path(args.root).resolve(strict=True)
    executable = Path(args.binary).resolve(strict=True)
    script = Path(__file__).resolve(strict=True)
    primary = f"{args.network}-listen-home" if args.role == "listen" else args.network
    command = [
        "docker", "run", "--rm", "--pull=never", "--name", f"{args.network}-{args.role}",
        "--network", primary, "--label", f"{LABEL}={args.network}",
        "--user", f"{os.getuid()}:{os.getgid()}", "--read-only", "--cap-drop=ALL",
        "--security-opt", "no-new-privileges", "--tmpfs", "/tmp:rw,nosuid,nodev,size=64m",
    ]
    for source, destination in (
        ("/usr", "/usr"), ("/usr/lib", "/lib"), ("/usr/lib", "/lib64"),
        ("/usr/bin", "/bin"), (str(executable), "/ssmcp"), (str(script), "/peer_netns.py"),
    ):
        command += ["--mount", f"type=bind,src={source},dst={destination},readonly"]
    command += ["--mount", f"type=bind,src={root},dst={root}", "--entrypoint", "/usr/bin/python3",
                "alpine:latest", "/peer_netns.py", "inside", "--root", str(root),
                "--role", args.role, "--origin", args.origin,
                "--target", args.target, "--listen", args.listen]
    os.execvp(command[0], command)


async def inside(args):
    root = Path(args.root)
    home = root / args.role
    active = set()
    servers = []

    async def bridge(reader, writer, destination):
        task = asyncio.current_task()
        active.add(task)
        remote_writer = None
        pumps = []
        try:
            if isinstance(destination, Path):
                remote_reader, remote_writer = await asyncio.open_unix_connection(str(destination))
            else:
                remote_reader, remote_writer = await asyncio.open_connection(*destination)

            async def pump(source, sink):
                while chunk := await source.read(65536):
                    sink.write(chunk)
                    await sink.drain()
                # Preserve the other direction after a TCP or Unix half-close.
                sink.write_eof()
                await sink.drain()

            pumps = [asyncio.create_task(pump(reader, remote_writer)),
                     asyncio.create_task(pump(remote_reader, writer))]
            await asyncio.gather(*pumps)
        except (ConnectionError, OSError):
            pass
        finally:
            for pump_task in pumps:
                pump_task.cancel()
            if pumps:
                await asyncio.gather(*pumps, return_exceptions=True)
            for stream in (writer, remote_writer):
                if stream is not None:
                    stream.close()
                    with contextlib.suppress(ConnectionError, OSError, TimeoutError):
                        await asyncio.wait_for(stream.wait_closed(), 1)
            active.discard(task)

    origin = urlsplit(args.origin)
    if origin.scheme != "http" or origin.hostname != "127.0.0.1" or not origin.port:
        raise ValueError("control fixture must use loopback HTTP")
    servers.append(await asyncio.start_server(
        lambda r, w: bridge(r, w, root / "control.sock"), "127.0.0.1", origin.port
    ))
    if args.role == "target":
        servers.append(await asyncio.start_server(
            lambda r, w: bridge(r, w, root / "target.sock"), *endpoint(args.target)
        ))
    else:
        servers.append(await asyncio.start_unix_server(
            lambda r, w: bridge(r, w, endpoint(args.listen)), path=str(root / "listen.sock")
        ))
    env = {
        "PATH": "/usr/bin:/bin", "HOME": str(home), "USER": "peer-fixture",
        "XDG_CONFIG_HOME": str(home / "config"), "SSMCP_AUTH_HOME": str(home / "auth"),
        "SSMCP_SESSION_HOME": str(home / "sessions"),
        "SSMCP_PEER_PORT_CONFIG": str(home / "peer.json"), "NO_COLOR": "1", "TERM": "dumb",
    }
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for signum in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(signum, stop.set)
    process = await asyncio.create_subprocess_exec(
        "/ssmcp", f"alice@{args.origin}",
        "--name", args.role, "--no-setup", "--input", "none", "--shell", "/bin/sh",
        cwd=home / "work", env=env, stdin=subprocess.DEVNULL,
    )
    waiter = asyncio.create_task(process.wait())
    stopper = asyncio.create_task(stop.wait())
    try:
        await asyncio.wait((waiter, stopper), return_when=asyncio.FIRST_COMPLETED)
        if process.returncode is None:
            process.send_signal(signal.SIGINT)
            try:
                await asyncio.wait_for(asyncio.shield(waiter), 5)
            except TimeoutError:
                process.kill()
                await waiter
        return process.returncode
    finally:
        stopper.cancel()
        await asyncio.gather(stopper, return_exceptions=True)
        for server in servers:
            server.close()
            await server.wait_closed()
        for task in list(active):
            task.cancel()
        await asyncio.gather(*list(active), return_exceptions=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    clean = commands.add_parser("cleanup")
    clean.add_argument("--network", required=True, type=owned_name)
    for action in ("client", "inside"):
        command = commands.add_parser(action)
        command.add_argument("--root", required=True)
        command.add_argument("--role", required=True, choices=("listen", "target"))
        command.add_argument("--origin", required=True)
        command.add_argument("--target", required=True)
        command.add_argument("--listen", required=True)
        if action == "client":
            command.add_argument("--network", required=True, type=owned_name)
            command.add_argument("--binary", required=True)
    args = parser.parse_args()
    if args.command == "cleanup":
        return cleanup(args)
    if args.command == "client":
        launch(args)
        return 1
    return asyncio.run(inside(args))


if __name__ == "__main__":
    sys.exit(main())
