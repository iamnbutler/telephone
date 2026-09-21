#!/usr/bin/env python3
"""Opt-in real runtime tests. Starts only owned sessions; never selects live peers.
Uses existing runtime auth and a private Telephone journal. Claude makes model
calls; normal CI must not invoke this script. No global config is modified.
"""
import argparse
import atexit
import json
import os
from pathlib import Path
import platform
import queue
import shutil
import subprocess
import tempfile
import threading
import time
import uuid


class Process:
    def __init__(self, command, env, cwd, log):
        self.error = open(log, "w")
        self.child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                      stderr=self.error, text=True, env=env, cwd=cwd,
                                      start_new_session=True)
        self.events = queue.Queue()
        self.sequence = 0
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        for line in self.child.stdout:
            try:
                self.events.put(json.loads(line))
            except ValueError:
                continue
        self.events.put({"_eof": True})

    def send(self, value):
        self.child.stdin.write(json.dumps(value) + "\n")
        self.child.stdin.flush()

    def until(self, predicate, timeout=90):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = self.events.get(timeout=max(0.01, deadline - time.monotonic()))
            if value.get("_eof"):
                raise RuntimeError("runtime exited; inspect its private stderr log")
            if predicate(value):
                return value
        raise TimeoutError("runtime response deadline")

    def rpc(self, method, params):
        self.sequence += 1
        ident = self.sequence
        self.send({"id": ident, "method": method, "params": params})
        value = self.until(lambda v: v.get("id") == ident)
        if "error" in value:
            raise RuntimeError(value["error"])
        return value["result"]

    def close(self):
        import signal
        if self.child.poll() is None:
            os.killpg(self.child.pid, signal.SIGTERM)
            try:
                self.child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(self.child.pid, signal.SIGKILL)
                self.child.wait(timeout=5)
        self.error.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true", help="authorize disposable native sessions and model calls")
    parser.add_argument("--binary", default="target/debug/telephone")
    parser.add_argument("--report", default="/tmp/telephone-compatibility.json")
    args = parser.parse_args()
    if not args.run:
        parser.error("pass --run to opt in to real authenticated runtime sessions")
    binary = str(Path(args.binary).resolve())
    report = {"platform": platform.platform(), "codex": subprocess.check_output(["codex", "--version"], text=True).strip(),
              "claude": subprocess.check_output(["claude", "--version"], text=True).strip(), "checks": []}
    root = Path(tempfile.mkdtemp(prefix="telephone-compat-")).resolve()
    root.chmod(0o700)
    report["private_logs"] = str(root)
    print("Private test directory:", root, flush=True)
    codex_home = root / "codex"
    codex_home.mkdir(mode=0o700)
    work = root / "work"
    work.mkdir()
    state = root / "state"
    atexit.register(lambda: (codex_home / "auth.json").unlink(missing_ok=True))
    atexit.register(lambda: (root / "claude-auth-settings.json").unlink(missing_ok=True))
    # Keep discovery, queues and credentials out of the user's Codex state.
    auth = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "auth.json"
    if auth.exists():
        shutil.copy2(auth, codex_home / "auth.json")
        (codex_home / "auth.json").chmod(0o600)
    env = os.environ.copy()
    for key in ["CODEX_THREAD_ID", "CLAUDE_PID", "CLAUDECODE", "TELEPHONE_ADDR", "TELEPHONE_NAME", "TELEPHONE_CODEX_INBOX", "CLAUDE_CODE_MESSAGING_SOCKET"]:
        env.pop(key, None)
    env.update(CODEX_HOME=str(codex_home), TELEPHONE_STATE_DIR=str(state))
    mcp_env = {"CODEX_HOME": str(codex_home), "TELEPHONE_STATE_DIR": str(state)}
    (codex_home / "config.toml").write_text(
        "[mcp_servers.telephone]\ncommand = " + json.dumps(binary) + '\nargs = ["mcp"]\nenv = { ' +
        ", ".join(k + " = " + json.dumps(v) for k, v in mcp_env.items()) + " }\n")
    settings_path = root / "claude-auth-settings.json"
    user_settings = Path(os.environ.get("CLAUDE_CONFIG_DIR", str(Path.home() / ".claude"))) / "settings.json"
    settings = json.loads(user_settings.read_text()) if user_settings.exists() else {}
    # Retain the existing auth helper and inbound policy, not unrelated hooks,
    # plugins or broad tool permissions. Never enable a disabled inbound policy.
    settings_path.write_text(json.dumps({k: settings[k] for k in ["apiKeyHelper", "crossSessionInbound"] if k in settings}))
    settings_path.chmod(0o600)
    processes = []

    def check(name, condition, detail=None):
        if not condition:
            raise AssertionError(name + (": " + str(detail) if detail else ""))
        report["checks"].append({"name": name, "result": "pass"})
        print("PASS", name, flush=True)

    def cli(address, *arguments):
        out = subprocess.run([binary, *arguments], env={**env, "TELEPHONE_ADDR": address},
                             text=True, capture_output=True, timeout=15)
        if out.returncode:
            raise RuntimeError(out.stderr)
        return out

    try:
        codex = Process(["codex", "app-server"], env, work, root / "codex.stderr")
        processes.append(codex)
        codex.rpc("initialize", {"clientInfo": {"name": "telephone-compat", "version": "0.2"}, "capabilities": {"experimentalApi": True}})
        codex.send({"method": "initialized"})
        threads = [codex.rpc("thread/start", {"cwd": str(work), "approvalPolicy": "never", "sandbox": "read-only",
                    "baseInstructions": "Only perform user-authorized Telephone nonce compatibility tests. No file changes. Message only exact test addresses explicitly supplied."})["thread"]["id"] for _ in range(2)]
        a, b = ["codex:" + tid for tid in threads]
        for tid in threads:
            codex.rpc("turn/start", {"threadId": tid, "input": [{"type": "text", "text": "Reply only READY. Do not call tools.", "text_elements": []}]})
            completed = codex.until(lambda v: v.get("method") == "turn/completed" and v.get("params", {}).get("threadId") == tid)
            check("Disposable Codex session initialized " + tid[-4:], completed["params"]["turn"]["status"] == "completed", completed)

        def tool(address, name, arguments=None):
            assert address in [a, b], "never call on an unowned Codex thread"
            result = codex.rpc("mcpServer/tool/call", {"threadId": address[6:], "server": "telephone", "tool": name, "arguments": arguments or {}})
            if result.get("isError"):
                raise RuntimeError(result)
            return result["content"][0]["text"]

        for address in [a, b]:
            check("Codex per-call identity " + address[-4:], json.loads(tool(address, "list_agents"))["you"] == address)
        # Send while the native host has admitted an active turn. Queue
        # acceptance remains separate from what the model eventually reads.
        busy = codex.rpc("turn/start", {"threadId": threads[1], "input": [{"type": "text", "text": "Reply only BUSY PROBE. Do not call tools.", "text_elements": []}]})
        check("Codex admits an active turn before the native send", busy["turn"]["status"] == "inProgress")
        native = tool(a, "send_message", {"to": b, "body": "compat-native-" + uuid.uuid4().hex, "kind": "inform"})
        check("Codex native queue acceptance", "Accepted by codex queue; not a read receipt" in native, native)
        empty = json.loads(tool(b, "check_inbox"))
        check("Accepted native send has no polling copy and explains the channel", empty["messages"] == [] and len(empty["notices"]) == 1)
        nonce = "compat-codex-poll-" + uuid.uuid4().hex
        tool(a, "send_message", {"to": b, "body": nonce, "kind": "request"})
        received = json.loads(cli(b, "inbox", "--json").stdout)
        check("Codex to Codex MCP/CLI request", len(received) == 1 and received[0]["body"] == nonce)
        tool(b, "send_message", {"to": a, "body": nonce + " reply", "kind": "reply", "reply_to": received[0]["id"]})
        check("Codex to Codex automatic reply", nonce in tool(a, "check_inbox"))
        # Expiry is observed against real time and the same sender MCP connection.
        time.sleep(16)
        expired = json.loads(cli(a, "doctor", b, "--json").stdout)
        check("Polling evidence expires without a sender restart", expired["polling"] is None and expired["preferred_transport"] == "queue")
        check("Expired peer returns to native delivery", "Accepted by codex queue" in tool(a, "send_message", {"to": b, "body": "compat-expiry-" + uuid.uuid4().hex}))

        config = json.dumps({"mcpServers": {"telephone": {"command": binary, "args": ["mcp"], "env": mcp_env}}})
        claudes = []
        system = "You are running user-authorized local Telephone compatibility tests. Use only Telephone tools, never edit files. Send only to exact test peer addresses supplied by the user or the sender of an incoming compat- request. Reply once to incoming requests with the requested nonce and kind reply/reply_to. Never reply to replies or informational messages."
        for index in range(2):
            command = ["claude", "--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                       "--session-id", str(uuid.uuid4()), "--name", "telephone-compat-" + str(index),
                       "--restricted", "--setting-sources", "", "--settings", str(settings_path),
                       "--strict-mcp-config", "--mcp-config", config, "--tools", "",
                       "--allowedTools", "mcp__telephone__list_agents,mcp__telephone__send_message,mcp__telephone__check_inbox",
                       "--permission-mode", "dontAsk", "--system-prompt", system]
            # Deliberately inherit an outer Codex identity: Claude must win.
            proc = Process(command, {**env, "CODEX_THREAD_ID": threads[0]}, work, root / f"claude-{index}.stderr")
            processes.append(proc)
            address = "claude:" + str(proc.child.pid)
            proc.send({"type": "user", "message": {"role": "user", "content": "Call Telephone list_agents once and report ONLY your own Telephone address. Do not message any peers."}})
            result = proc.until(lambda v: v.get("type") == "result")
            check("Claude native identity overrides inherited Codex ID " + str(index), not result.get("is_error") and address in result.get("result", ""), result.get("result"))
            claudes.append((proc, address))
        c, d = [pair[1] for pair in claudes]

        def ask(index, content):
            claudes[index][0].send({"type": "user", "message": {"role": "user", "content": content}})

        def wait_message(address, nonce, seconds=90):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                messages = json.loads(cli(address, "inbox", "--json").stdout)
                for message in messages:
                    if nonce in message["body"]:
                        return message
                time.sleep(2)
            raise TimeoutError("No matching message for " + nonce)

        nonce = "compat-codex-claude-" + uuid.uuid4().hex
        sent = tool(a, "send_message", {"to": c, "body": "Reply once with " + nonce, "kind": "request"})
        check("Codex to idle Claude native write", "Written over uds" in sent, sent)
        reply = wait_message(a, nonce)
        check("Claude model reads native request and replies to Codex inbox", reply["kind"] == "reply" and reply["from"] == c)
        claudes[0][0].until(lambda v: v.get("type") == "result")

        nonce = "compat-claude-codex-" + uuid.uuid4().hex
        tool(b, "check_inbox")
        ask(0, f"Send {b} a request whose body is exactly {nonce}. Then finish this turn; I will ask you to check the reply.")
        incoming = wait_message(b, nonce)
        tool(b, "send_message", {"to": c, "body": nonce + " reply", "kind": "reply", "reply_to": incoming["id"]})
        claudes[0][0].until(lambda v: v.get("type") == "result")
        ask(0, "Call check_inbox and report the matching reply nonce. Do not acknowledge it.")
        result = claudes[0][0].until(lambda v: v.get("type") == "result")
        check("Claude to Codex request and automatic Claude return path", nonce in result.get("result", ""), result.get("result"))

        # Let the receiving Claude remain idle; do not advertise its inbox.
        nonce = "compat-claude-claude-" + uuid.uuid4().hex
        ask(0, f"Send {d} a request asking it to reply once with {nonce}. Finish after sending.")
        # Keep the sender's polling advertisement current while its peer works.
        reply = wait_message(c, nonce)
        check("Claude to Claude native request and inbox reply", reply["kind"] == "reply" and reply["from"] == d)
        check("Owned Claude session discovery", all(pair[1] in tool(a, "list_agents") for pair in claudes))
        report["status"] = "passed"
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
        raise
    finally:
        for process in reversed(processes):
            process.close()
        (codex_home / "auth.json").unlink(missing_ok=True)
        settings_path.unlink(missing_ok=True)
        Path(args.report).write_text(json.dumps(report, indent=2) + "\n")
        print("Report:", args.report, flush=True)


if __name__ == "__main__":
    main()
