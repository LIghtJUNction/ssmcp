"""Verify session.new and session.cd against a loopback relay and production gateway."""
import argparse
import json
import os
from pathlib import Path
import tempfile
import time
import urllib.request
import uuid

from e2e_account_relay import Harness, wait_for


def run(binary):
    with tempfile.TemporaryDirectory(prefix="ssmcp-workbench-") as tmp:
        root = Path(tmp)
        test = Harness(root, binary)
        workspace = root / "B"
        sub = workspace / "project"
        nested = sub / "child only"
        nested.mkdir(parents=True)
        # Keep all private runtime files and session manifests inside the fixture.
        base_env = test.env
        def env(device):
            result = base_env(device)
            result.update(HOME=str(root / device),
                          XDG_STATE_HOME=str(root / device / "state"),
                          XDG_DATA_HOME=str(root / device / "data"),
                          XDG_CACHE_HOME=str(root / device / "cache"),
                          PATH=os.defpath, SHELL="/bin/sh")
            return result
        test.env = env
        try:
            parent = test.unsigned("primary")
            test.connection("primary")
            pairing = workspace / "auth" / "pairing.json"
            test.spawn("B", "gui", "serve", "--pairing-file", str(pairing))
            wait_for(pairing.exists, "gateway pairing")
            pair = json.loads(pairing.read_text())
            origin = pair["url"]
            def http(path, data=None):
                request = urllib.request.Request(
                    origin + "/api/gui/v1/" + path,
                    data=None if data is None else json.dumps(data).encode(),
                    headers={"Authorization": "Bearer " + pair["token"],
                             "Origin": origin, "Content-Type": "application/json"},
                )
                with urllib.request.urlopen(request, timeout=15) as response:
                    return json.load(response)
            def attach(target):
                entry = http("connections", {"session":target, "request_id":uuid.uuid4().hex,
                                               "read_only":False})
                return entry["connection_id"]
            def bridge(connection, op, **kw):
                receipt = http("connections/" + connection + "/bridge", {
                    "id": uuid.uuid4().hex, "version":1, "operation":{"op":op, **kw}
                })
                assert receipt["body"]["kind"] != "error", receipt["body"]
                return receipt
            def active(connection):
                response = bridge(connection, "capabilities")
                names = {item["id"] for item in response["body"]["value"]["commands"]}
                return response["generation"] if {"session.new","session.cd"} <= names else None
            def action(connection, generation, command, **arguments):
                return bridge(connection, "command", generation=generation, command=command,
                              arguments=arguments)["body"]["value"]["value"]
            def state(connection, generation):
                return bridge(connection, "state", generation=generation,resource="runtime",
                              query={})["body"]["value"]["value"]
            def pwd(peer, directory):
                job = peer.call("ssmcp.exec.start", {"command":["/bin/pwd"],"mode":"pipes"})
                peer.call("ssmcp.exec.wait", {"job_id":job["job_id"],"wait_ms":5000})
                read = peer.call("ssmcp.exec.read", {"job_id":job["job_id"]})
                assert read["exit_code"] == 0 and read["stdout"].strip() == str(directory), read
            targets = http("sessions")["sessions"]
            first = next(row["target"] for row in targets if row["name"] == "primary")
            primary = attach(first)
            pgen = wait_for(lambda: active(primary),"primary session bridge")
            assert state(primary, pgen)["process_id"] == parent.pid
            assert action(primary,pgen,"session.cd",path="project")["cwd"] == str(sub)
            pwd(test.connection("primary"),sub)
            created = action(primary,pgen,"session.new")
            assert created["session"] != first and created["cwd"] == str(sub)
            secondary = attach(created["session"])
            sgen = wait_for(lambda: active(secondary), "child session bridge", timeout=15)
            second_state = state(secondary,sgen)
            assert second_state["process_id"] != parent.pid
            assert second_state["client_id"] != state(primary,pgen)["client_id"]
            assert second_state["cwd"] == str(sub)
            assert action(secondary,sgen,"session.cd",path="child only")["cwd"] == str(nested)
            pwd(test.state.sessions[second_state["client_id"]],nested)
            pwd(test.connection("primary"),sub)
            assert state(primary,pgen)["cwd"] == str(sub)
            print("PASS: distinct process, relay identity and private attach target; child inherits cwd; both sessions execute pwd in their own directories", flush=True)
        except Exception:
            for _, err, _, _ in test.logs:
                print(test.redacted(err.read_text(errors="replace")[-1500:]), flush=True)
            print("fixture errors:", test.state.errors)
            raise
        finally:
            test.cleanup()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    run(parser.parse_args().binary.resolve())
