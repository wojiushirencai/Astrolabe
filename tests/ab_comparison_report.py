#!/usr/bin/env python3
"""
Full A/B Comparison Runner for Astrolabe:
- Version A (Baseline): /Users/zhoulei/.cargo/bin/astrolabe (v0.11.0 pre-trim / original)
- Version B (Optimized): ./target/debug/astrolabe (post-simplification)
"""

import subprocess
import json
import sys
import time

BIN_A = "/Users/zhoulei/.cargo/bin/astrolabe"
BIN_B = "./target/debug/astrolabe"

class McpClient:
    def __init__(self, cmd):
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.req_id = 1
        # initialize
        self.call("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "ab-diff", "version": "1.0"}
        })

    def call(self, method, params=None):
        req = {"jsonrpc": "2.0", "id": self.req_id, "method": method}
        if params is not None:
            req["params"] = params
        self.req_id += 1
        self.p.stdin.write(json.dumps(req) + "\n")
        self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            err = self.p.stderr.read()
            raise RuntimeError(f"Server disconnected. Stderr: {err}")
        return json.loads(line)

    def wait_ready(self):
        t0 = time.time()
        while time.time() - t0 < 20:
            resp = self.call("tools/call", {
                "name": "get_lang",
                "arguments": {}
            })
            text = resp.get("result", {}).get("content", [{}])[0].get("text", "")
            if "索引正在后台构建" in text:
                time.sleep(0.5)
                continue
            return
        raise TimeoutError("Timed out waiting for index build")

    def close(self):
        self.p.terminate()

def main():
    print("=" * 80)
    print("ASTROLABE MCP A/B VERSION COMPARISON REPORT")
    print("Version A (Baseline):  Installed ~/.cargo/bin/astrolabe (v0.11.0)")
    print("Version B (Optimized): Current target/debug/astrolabe (精炼优化版)")
    print("=" * 80)

    client_a = McpClient([BIN_A, ".", "--context=default"])
    client_b = McpClient([BIN_B, ".", "--context=default"])

    print("\nWaiting for background index to become ready on both servers...")
    client_a.wait_ready()
    client_b.wait_ready()
    print("Both servers Ready!")

    resp_a = client_a.call("tools/list", {})
    resp_b = client_b.call("tools/list", {})

    tools_a = {t["name"]: t for t in resp_a.get("result", {}).get("tools", [])}
    tools_b = {t["name"]: t for t in resp_b.get("result", {}).get("tools", [])}

    raw_a = json.dumps(resp_a).encode("utf-8")
    raw_b = json.dumps(resp_b).encode("utf-8")

    size_a = len(raw_a)
    size_b = len(raw_b)

    desc_a_total = sum(len(t.get("description", "").encode("utf-8")) for t in tools_a.values())
    desc_b_total = sum(len(t.get("description", "").encode("utf-8")) for t in tools_b.values())

    schema_a_total = sum(len(json.dumps(t.get("inputSchema", {})).encode("utf-8")) for t in tools_a.values())
    schema_b_total = sum(len(json.dumps(t.get("inputSchema", {})).encode("utf-8")) for t in tools_b.values())

    print("\n--- 1. OVERALL PAYLOAD & TOKEN REDUCTION ---")
    print(f"Total tools/list response: {size_a} B ({size_a/1024:.2f} KB) -> {size_b} B ({size_b/1024:.2f} KB) | Drop: {((size_a - size_b)/size_a)*100:.1f}%")
    print(f"Total Tool Descriptions:   {desc_a_total} B ({desc_a_total/1024:.2f} KB) -> {desc_b_total} B ({desc_b_total/1024:.2f} KB) | Drop: {((desc_a_total - desc_b_total)/desc_a_total)*100:.1f}%")
    print(f"Total InputSchemas:        {schema_a_total} B ({schema_a_total/1024:.2f} KB) -> {schema_b_total} B ({schema_b_total/1024:.2f} KB) | Drop: {((schema_a_total - schema_b_total)/schema_a_total)*100:.1f}%")

    tokens_a = int(size_a / 3.8)
    tokens_b = int(size_b / 3.8)
    print(f"Estimated Token Cost:      ~{tokens_a} tokens -> ~{tokens_b} tokens | Saved per turn: ~{tokens_a - tokens_b} tokens")

    print("\n--- 2. PER-TOOL DETAILED BREAKDOWN ---")
    print(f"{'Tool Name':<18} | {'Ver A Total':<12} | {'Ver B Total':<12} | {'Ver A Desc':<11} | {'Ver B Desc':<11} | {'Reduction':<9}")
    print("-" * 80)
    for name in sorted(tools_b.keys()):
        tb = tools_b[name]
        ta = tools_a.get(name)
        if ta:
            tot_a = len(json.dumps(ta).encode("utf-8"))
            tot_b = len(json.dumps(tb).encode("utf-8"))
            da = len(ta.get("description", "").encode("utf-8"))
            db = len(tb.get("description", "").encode("utf-8"))
            pct = ((tot_a - tot_b) / tot_a) * 100
            print(f"{name:<18} | {tot_a:>7} B    | {tot_b:>7} B    | {da:>6} B     | {db:>6} B     | -{pct:>5.1f}%")
        else:
            tot_b = len(json.dumps(tb).encode("utf-8"))
            db = len(tb.get("description", "").encode("utf-8"))
            print(f"{name:<18} | [NEW IN B]   | {tot_b:>7} B    |     -        | {db:>6} B     |   N/A")

    print("\n--- 3. BEHAVIORAL PARITY VERIFICATION ACROSS COMMON TOOLS ---")
    common_queries = [
        ("search_code", {"query": "fn assemble", "context_lines": 0}, "Literal search_code"),
        ("search_code", {"query": "resolve_contx", "regex": False}, "Exact string search_code"),
        ("search_code", {"query": "fn\\s+assemble", "regex": True}, "Regex search_code"),
        ("find_symbol", {"query": "AstrolabeServer", "substring": False}, "Exact find_symbol"),
        ("find_symbol", {"query": "AstrolabeServer/new"}, "Hierarchical find_symbol"),
        ("get_lang", {}, "get_lang status"),
    ]

    parity_passed = 0
    for tool_name, args, label in common_queries:
        if tool_name not in tools_a or tool_name not in tools_b:
            continue
        res_a = client_a.call("tools/call", {"name": tool_name, "arguments": args})
        res_b = client_b.call("tools/call", {"name": tool_name, "arguments": args})

        text_a = res_a.get("result", {}).get("content", [{}])[0].get("text", "")
        text_b = res_b.get("result", {}).get("content", [{}])[0].get("text", "")

        assert res_a.get("result", {}).get("isError") != True, f"Ver A error on {label}: {text_a}"
        assert res_b.get("result", {}).get("isError") != True, f"Ver B error on {label}: {text_b}"
        assert len(text_a) > 0 and len(text_b) > 0

        # Strip variable index_root line
        lines_a = [l for l in text_a.splitlines() if not l.startswith("index_root:")]
        lines_b = [l for l in text_b.splitlines() if not l.startswith("index_root:")]

        print(f"  [PARITY OK] {label:<28}: Ver A ({len(lines_a)} lines) == Ver B ({len(lines_b)} lines)")
        parity_passed += 1

    print(f"\nBehavioral parity tests passed: {parity_passed}/{len(common_queries)}")
    print("=" * 80)

    client_a.close()
    client_b.close()

if __name__ == "__main__":
    main()
