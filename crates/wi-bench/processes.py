"""Process measurements, OS caches warm. Requires a prepared, indexed corpus.

All configuration/cache paths go to the provided scratch directory. Fresh
process != cold disk. CLI ready uses a real read (open + checksummed file read,
no stat walk); CLI query includes catch-up/eligibility and is reported apart.
MCP ready is initialize response; idle RSS is sampled after background catch-up.
"""
import argparse
import json
import hashlib
import os
from pathlib import Path
import selectors
import subprocess
import time


def metric(values, unit="ms", target=None):
    values = sorted(values)
    import math
    p95 = values[math.ceil(len(values) * .95) - 1]
    return dict(unit=unit, n=len(values), p50=values[math.ceil(len(values)*.5)-1],
                p95=p95, max=values[-1], target=target,
                pass_=None if target is None else p95 <= target)


def clean_metric(values, unit="ms", target=None):
    result = metric(values, unit, target)
    result["pass"] = result.pop("pass_")
    return result



def tool_result(response):
    """Envelope of a tool result: one TextContent block of compact JSON, no
    structuredContent (docs/PROTOCOL.md). An error result fails the run."""
    assert not response.get("isError"), response
    content = response.get("content", [])
    assert len(content) == 1 and content[0].get("type") == "text", response
    return json.loads(content[0]["text"])


class Mcp:
    def __init__(self, base, env, stderr_path):
        self.err = stderr_path.open("w")
        self.p = subprocess.Popen(base+["mcp"], env=env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=self.err, text=True)
        self.id = 0
        try:
            self.request("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                       "clientInfo": {"name": "wi-bench", "version": "1"}})
            self.send(dict(jsonrpc="2.0", method="notifications/initialized"))
        except BaseException:
            self.close()
            raise

    def send(self, value):
        self.p.stdin.write(json.dumps(value)+"\n")
        self.p.stdin.flush()

    def request(self, method, params):
        self.id += 1
        self.send(dict(jsonrpc="2.0", id=self.id, method=method, params=params))
        selector = selectors.DefaultSelector()
        selector.register(self.p.stdout, selectors.EVENT_READ)
        try:
            while True:
                assert selector.select(120), "MCP request timed out"
                response = json.loads(self.p.stdout.readline())
                if response.get("id") == self.id:
                    assert "result" in response, response
                    return response["result"]
        finally:
            selector.close()

    def call(self, name, **args):
        return tool_result(self.request("tools/call", dict(name=name, arguments=args)))

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.p.wait()
        self.err.close()


def tool_metrics(root, scratch, base, env):
    metrics = {}
    config = root / ".wiki-island/config.toml"
    original = config.read_text()
    try:
        for mode in ("standard", "full"):
            lines = [line for line in original.splitlines() if not line.startswith("durability =")]
            config.write_text("\n".join(lines)+f'\ndurability = "{mode}"\n')
            client = Mcp(base, env, scratch / f"tools-{mode}.stderr")
            try:
                time.sleep(3)
                durations = []
                for i in range(210):
                    # Server flushes after sending each reply. An untimed read
                    # serializes after the previous request's flush, so timed
                    # writes don't wait for the preceding operation's commit.
                    client.call("wi_read", identifier="source.md")
                    text = f"---\ntitle: MCP Save {i}\ntype: note\n---\nSynthetic searchable save {i}. 결제 환불 日本語。\n"
                    start = time.perf_counter()
                    receipt = client.call("wi_write", identifier=f"mcp-{mode}/save-{i}.md", content=text)
                    elapsed = (time.perf_counter()-start)*1000
                    assert receipt["index"] == "ok", receipt
                    assert not receipt["diagnostics"], receipt
                    if i >= 10:
                        durations.append(elapsed)
                metrics[f"save_mcp_{mode}"] = clean_metric(durations, target=50)
                if mode == "standard":
                    for name, tool, args, target in (
                            ("search_mcp_current_warm", "wi_search", {"query": "payment", "limit": 8}, 50),
                            # Natural-language paths: ranked multi-term fill
                            # (English, Korean) and broad strict matches.
                            ("search_mcp_ranked_en", "wi_search", {"query": "payment refund", "limit": 8}, 50),
                            ("search_mcp_ranked_partial_en", "wi_search", {"query": "payment nonexistentword", "limit": 8}, 50),
                            ("search_mcp_ranked_ko", "wi_search", {"query": "결제 환불", "limit": 8}, 50),
                            ("search_mcp_ranked_ko_phrase", "wi_search", {"query": "환불 정책이", "limit": 8}, 50),
                            ("search_mcp_strict_ko_not", "wi_search", {"query": "환불 NOT 정책이", "limit": 8}, 50),
                            ("context_mcp_depth_2", "wi_context", {"identifier": "notes/000/n00000.md", "depth": 2}, 100)):
                        durations = []
                        for i in range(210):
                            start = time.perf_counter()
                            result = client.call(tool, **args)
                            elapsed = (time.perf_counter()-start)*1000
                            assert result["hits"], result
                            assert not any(d.get("code") == "generation-changed" for d in result["diagnostics"]), result
                            if i >= 10:
                                durations.append(elapsed)
                        metrics[name] = clean_metric(durations, target=target)
            finally:
                client.close()
    finally:
        config.write_text(original)
    return metrics


def measure(args):
    wi = str(Path(args.wi).resolve())
    root = Path(args.root).resolve()
    cache = Path(args.cache).resolve()
    scratch = root.parent
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("WI_", "XDG_", "HF_"))}
    env.update(HOME=str(scratch / "process-home"),
               USERPROFILE=str(scratch / "process-home"),
               APPDATA=str(scratch / "process-roaming"),
               LOCALAPPDATA=str(scratch / "process-local"),
               WI_CACHE_DIR=str(cache),
               XDG_CONFIG_HOME=str(scratch / "process-config"),
               XDG_DATA_HOME=str(scratch / "process-data"),
               XDG_CACHE_HOME=str(scratch / "process-cache"))
    base = [wi, "--vault", str(root)]
    def run(cmd):
        start = time.perf_counter()
        result = subprocess.run(cmd, env=env, capture_output=True, timeout=120)
        elapsed = (time.perf_counter()-start)*1000
        assert result.returncode == 0, result.stderr.decode()
        json.loads(result.stdout)
        return elapsed

    report_path = Path(args.report)
    report = json.loads(report_path.read_text())
    files = sorted(root.rglob("*.md"))
    tree = hashlib.sha256()
    for path in files:
        tree.update(str(path.relative_to(root)).encode()+b"\0")
        tree.update(("sha256:"+hashlib.sha256(path.read_bytes()).hexdigest()+"\n").encode())
    assert len(files) == report["corpus"]["notes"], "process vault must have the exact initial note count; use wi-bench prepare"
    assert "sha256:"+tree.hexdigest() == report["corpus"]["tree_sha256"], "process corpus fingerprint differs"
    metrics = {}
    metrics["startup_cli_read"] = clean_metric(
        [run(base+["--json", "read", "source.md"]) for _ in range(30)], target=300)
    metrics["cli_query_with_catch_up"] = clean_metric(
        [run(base+["--json", "search", "payment"]) for _ in range(20)])
    startups, rss = [], []
    for attempt in range(5):
        start = time.perf_counter()
        with (scratch / f"mcp-{attempt}.stderr").open("w") as err:
            p = subprocess.Popen(base+["mcp"], env=env, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=err, text=True)
            try:
                request = dict(jsonrpc="2.0", id=1, method="initialize", params={
                    "protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "wi-bench", "version": "1"}})
                p.stdin.write(json.dumps(request)+"\n")
                p.stdin.flush()
                selector = selectors.DefaultSelector()
                selector.register(p.stdout, selectors.EVENT_READ)
                assert selector.select(120), "MCP initialize timed out"
                response = json.loads(p.stdout.readline())
                selector.close()
                assert response.get("id") == 1 and "result" in response, response
                startups.append((time.perf_counter()-start)*1000)
                p.stdin.write(json.dumps(dict(jsonrpc="2.0", method="notifications/initialized"))+"\n")
                p.stdin.flush()
                # Let watcher/reconciliation/export allocation settle. Report
                # total process RSS, including the service's threads/allocator.
                time.sleep(3)
                assert p.poll() is None, "MCP exited before RSS measurement"
                for _ in range(5):
                    output = subprocess.check_output(["ps", "-o", "rss=", "-p", str(p.pid)], text=True)
                    rss.append(int(output.strip())*1024/1_000_000)
                    time.sleep(.2)
            finally:
                p.stdin.close()
                try:
                    p.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait()
    metrics["startup_mcp_initialize"] = clean_metric(startups, target=300)
    metrics["idle_mcp_rss"] = clean_metric(rss, "MB", 100)
    metrics["binary_size"] = clean_metric([Path(wi).stat().st_size/1_000_000], "MB", 30)
    writes = []
    for i in range(22):
        text = f"---\ntitle: Save {i}\ntype: note\n---\nSynthetic searchable save {i}. 결제 환불 日本語。\n"
        elapsed = run(base+["--json", "write", f"process/save-{i}.md", "--content", text])
        if i >= 2:
            writes.append(elapsed)
    metrics["write_cli_including_flush"] = clean_metric(writes)
    metrics.update(tool_metrics(root, scratch, base, env))
    report["metrics"].update(metrics)
    report_path.write_text(json.dumps(report, indent=2)+"\n")
    rows = [f"Machine: {report['machine']}. Corpus: {report['corpus']['version']}, {report['corpus']['notes']} initial notes.", "", "| Metric | n | p50 | p95 | Unit | Target | Pass |", "| --- | ---: | ---: | ---: | --- | ---: | --- |"]
    for name, value in sorted(report["metrics"].items()):
        verdict = "—" if value["pass"] is None else ("PASS" if value["pass"] else "FAIL")
        target = "—" if value["target"] is None else str(value["target"])
        rows.append(f"| {name} | {value['n']} | {value['p50']:.3f} | {value['p95']:.3f} | {value['unit']} | {target} | {verdict} |")
    report_path.with_suffix(".md").write_text("\n".join(rows)+"\n")
    print(json.dumps(metrics, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("wi", "root", "cache", "report"):
        parser.add_argument("--"+arg, required=True)
    measure(parser.parse_args())
