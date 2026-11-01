#!/usr/bin/env python3
"""
Comprehensive Test and Verification Suite for Astrolabe Release Binary (v0.17.4).
Tests:
1. Schema and tools/list verification (canonical 7 names only, no aliases leaked)
2. All 7 canonical tools invocation
3. Backwards-compatible aliases transparency redirection
4. High frequency & concurrency stress testing (latency metrics, memory, stability)
5. Global CLI and npx shim verification
"""

import json
import os
import subprocess
import sys
import time
import threading
from concurrent.futures import ThreadPoolExecutor
import psutil

TARGET_BIN = r"C:\Users\z3437\project\Astrolabe\target\release\astrolabe.exe"
PROJECT_DIR = r"C:\Users\z3437\project\Astrolabe"

CANONICAL_7 = [
    "search_code",
    "find_symbol",
    "resolve_contx",
    "get_diag",
    "get_lang",
    "find_refs",
    "goto_def",
]

ALIASES_TO_TEST = [
    ("resolve_context", "resolve_contx", {"task_description": "refactor aliases"}),
    ("ast_resolve_contx", "resolve_contx", {"task_description": "refactor aliases"}),
    ("ast_context", "resolve_contx", {"task_description": "refactor aliases"}),
    ("get_diagnostics", "get_diag", {"path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("ast_get_diag", "get_diag", {"path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("ast_diag", "get_diag", {"path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("get_languages", "get_lang", {}),
    ("ast_get_lang", "get_lang", {}),
    ("ast_langs", "get_lang", {}),
    ("ast_lang", "get_lang", {}),
    ("find_references", "find_refs", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("ast_find_ref", "find_refs", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("ast_find_refs", "find_refs", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("goto_definition", "goto_def", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("ast_goto_def", "goto_def", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("ast_def", "goto_def", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
    ("ast_search_code", "search_code", {"query": "TOOL_ALIASES"}),
    ("ast_search", "search_code", {"query": "TOOL_ALIASES"}),
    ("ast_find_symbol", "find_symbol", {"query": "AstrolabeServer"}),
    ("ast_symbol", "find_symbol", {"query": "AstrolabeServer"}),
    ("explain_symbol", "find_symbol", {"name": "AstrolabeServer"}),
]

class McpProcess:
    def __init__(self, cmd, cwd=PROJECT_DIR):
        self.cmd = cmd
        self.cwd = cwd
        is_win = sys.platform == "win32"
        shell = is_win and (isinstance(cmd, list) and cmd[0].lower().endswith((".cmd", ".bat")))
        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            cwd=cwd,
            text=True,
            encoding="utf-8",
            errors="replace",
            shell=shell,
        )
        self.ps_proc = psutil.Process(self.proc.pid)
        self.req_id = 0
        self.lock = threading.Lock()

    def send_request(self, method, params=None):
        with self.lock:
            self.req_id += 1
            curr_id = self.req_id
            payload = {"jsonrpc": "2.0", "id": curr_id, "method": method}
            if params is not None:
                payload["params"] = params
            req_str = json.dumps(payload) + "\n"
            t0 = time.perf_counter()
            self.proc.stdin.write(req_str)
            self.proc.stdin.flush()
            line = self.proc.stdout.readline()
            elapsed_ms = (time.perf_counter() - t0) * 1000.0
            if not line:
                raise RuntimeError(f"Process terminated prematurely (returncode={self.proc.poll()})")
            resp = json.loads(line)
            return resp, elapsed_ms

    def init_handshake(self):
        resp, _ = self.send_request("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "astrolabe-tester", "version": "1.0"}
        })
        if "error" in resp:
            raise RuntimeError(f"Handshake error: {resp['error']}")
        return resp

    def wait_indexed(self, max_seconds=30):
        t0 = time.time()
        while time.time() - t0 < max_seconds:
            resp, _ = self.send_request("tools/call", {
                "name": "get_lang",
                "arguments": {}
            })
            res = resp.get("result", {})
            content = res.get("content", [])
            text = content[0].get("text", "") if content else ""
            if res.get("isError") or "building in the background" in text.lower() or "索引正在后台构建" in text:
                time.sleep(0.3)
                continue
            return
        raise TimeoutError("Index build timed out")

    def memory_info(self):
        try:
            mem = self.ps_proc.memory_info()
            return {
                "rss_mb": mem.rss / (1024 * 1024),
                "vms_mb": mem.vms / (1024 * 1024),
            }
        except Exception:
            return {"rss_mb": 0.0, "vms_mb": 0.0}

    def close(self):
        try:
            self.proc.terminate()
            self.proc.wait(timeout=3)
        except Exception:
            self.proc.kill()


def test_schema_and_list():
    print("\n[TEST 1] Schema & tools/list Canonical Verification")
    client = McpProcess([TARGET_BIN, ".", "--context=default"])
    try:
        client.init_handshake()
        resp, elapsed_ms = client.send_request("tools/list", {})
        result = resp.get("result", {})
        tools = result.get("tools", [])
        tool_names = [t["name"] for t in tools]
        print(f"  tools/list returned in {elapsed_ms:.2f} ms")
        print(f"  Tool count: {len(tool_names)}")
        print(f"  Tool names: {tool_names}")

        expected_set = set(CANONICAL_7)
        actual_set = set(tool_names)

        assert len(tools) == 7, f"Expected exactly 7 tools, got {len(tools)}"
        assert actual_set == expected_set, f"Tool set mismatch: expected {expected_set}, got {actual_set}"

        # Verify no aliases leaked into tools/list
        for alias, _, _ in ALIASES_TO_TEST:
            assert alias not in actual_set, f"Alias {alias} leaked into tools/list!"

        # Verify inputSchema
        for t in tools:
            name = t["name"]
            desc = t.get("description", "")
            schema = t.get("inputSchema", {})
            annotations = t.get("annotations", {})
            assert desc, f"Tool {name} missing description"
            assert schema.get("type") == "object", f"Tool {name} schema not an object"
            assert "properties" in schema, f"Tool {name} missing properties in schema"
            # verify read_only annotation
            assert annotations.get("readOnlyHint") is True, f"Tool {name} missing readOnlyHint annotation"
            print(f"  [OK] Tool '{name}' schema valid (properties: {list(schema['properties'].keys())[:4]}...)")

        print("  -> Schema & tools/list check: PASS")
        return True
    finally:
        client.close()


def test_canonical_7_tools():
    print("\n[TEST 2] 7 Canonical Tools Full-Coverage Functional Invocation")
    client = McpProcess([TARGET_BIN, ".", "--context=default"])
    try:
        client.init_handshake()
        client.wait_indexed()

        test_cases = [
            ("resolve_contx", {"task_description": "implement tool simplification and alias compatibility"}),
            ("search_code", {"query": "canonical_tool_name"}),
            ("find_symbol", {"query": "AstrolabeServer"}),
            ("find_refs", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
            ("goto_def", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
            ("get_diag", {"path": "crates/astrolabe-mcp/src/compat.rs"}),
            ("get_lang", {}),
        ]

        results = {}
        for name, args in test_cases:
            resp, elapsed_ms = client.send_request("tools/call", {
                "name": name,
                "arguments": args
            })
            assert "error" not in resp, f"Tool {name} returned error: {resp.get('error')}"
            res = resp.get("result", {})
            is_error = res.get("isError", False)
            assert not is_error, f"Tool {name} returned isError=True"
            content = res.get("content", [])
            assert len(content) > 0, f"Tool {name} returned empty content"
            text = content[0].get("text", "")
            assert len(text) > 0, f"Tool {name} returned empty text"
            assert "thread 'main' panicked" not in text, f"Tool {name} panicked: {text}"
            assert text.startswith("index_root:"), f"Tool {name} missing 'index_root:' header prefix"

            results[name] = {
                "elapsed_ms": elapsed_ms,
                "chars": len(text),
                "summary": text.splitlines()[0] if text else "",
            }
            print(f"  [OK] {name:<14} -> {elapsed_ms:>6.2f} ms | {len(text):>6} chars | {results[name]['summary']}")

        print("  -> All 7 canonical tools: PASS")
        return results
    finally:
        client.close()


def test_aliases_compatibility():
    print("\n[TEST 3] Backward-Compatible Aliases Verification")
    client = McpProcess([TARGET_BIN, ".", "--context=default"])
    try:
        client.init_handshake()
        client.wait_indexed()

        alias_results = []
        for alias, canonical, args in ALIASES_TO_TEST:
            resp, elapsed_ms = client.send_request("tools/call", {
                "name": alias,
                "arguments": args
            })
            assert "error" not in resp, f"Alias {alias} returned RPC error: {resp.get('error')}"
            res = resp.get("result", {})
            is_error = res.get("isError", False)
            assert not is_error, f"Alias {alias} returned isError=True"
            content = res.get("content", [])
            assert len(content) > 0, f"Alias {alias} returned empty content"
            text = content[0].get("text", "")
            assert len(text) > 0, f"Alias {alias} returned empty text"
            assert "thread 'main' panicked" not in text, f"Alias {alias} panicked: {text}"

            alias_results.append({
                "alias": alias,
                "canonical": canonical,
                "elapsed_ms": elapsed_ms,
                "chars": len(text),
                "status": "PASS"
            })
            print(f"  [OK] Alias '{alias}' -> mapped to '{canonical}' ({elapsed_ms:.2f} ms, {len(text)} chars)")

        print(f"  -> All {len(ALIASES_TO_TEST)} aliases verified: PASS")
        return alias_results
    finally:
        client.close()


def test_stress_and_concurrency():
    print("\n[TEST 4] Windows High-Frequency & Concurrency Stress Test")
    client = McpProcess([TARGET_BIN, ".", "--context=default"])
    try:
        client.init_handshake()
        client.wait_indexed()

        mem_start = client.memory_info()
        print(f"  Initial Memory: RSS = {mem_start['rss_mb']:.2f} MB")

        # 1. 100 high-frequency sequential requests with varied tools
        queries = [
            ("search_code", {"query": "AstrolabeServer"}),
            ("find_symbol", {"query": "AstrolabeServer"}),
            ("find_symbol", {"query": "TOOL_ALIASES"}),
            ("search_code", {"query": "canonical_tool_name"}),
            ("resolve_contx", {"task_description": "optimize indexing speed"}),
            ("get_lang", {}),
            ("get_diag", {"path": "crates/astrolabe-mcp/src/compat.rs"}),
            ("find_refs", {"symbol": "canonical_tool_name", "path": "crates/astrolabe-mcp/src/compat.rs"}),
            ("goto_def", {"symbol": "TOOL_ALIASES", "path": "crates/astrolabe-mcp/src/compat.rs"}),
            ("ast_search", {"query": "assemble"}),
        ]

        total_requests = 100
        latencies = []
        t_start = time.perf_counter()

        for i in range(total_requests):
            tool_name, args = queries[i % len(queries)]
            resp, elapsed_ms = client.send_request("tools/call", {
                "name": tool_name,
                "arguments": args
            })
            assert "error" not in resp, f"Request {i+1} failed: {resp.get('error')}"
            latencies.append(elapsed_ms)

        total_elapsed = time.perf_counter() - t_start
        mem_end = client.memory_info()

        latencies.sort()
        avg_ms = sum(latencies) / len(latencies)
        median_ms = latencies[len(latencies) // 2]
        p95_ms = latencies[int(len(latencies) * 0.95)]
        p99_ms = latencies[int(len(latencies) * 0.99)]
        rps = total_requests / total_elapsed

        stats = {
            "total_requests": total_requests,
            "total_time_s": total_elapsed,
            "rps": rps,
            "min_ms": latencies[0],
            "max_ms": latencies[-1],
            "avg_ms": avg_ms,
            "median_ms": median_ms,
            "p95_ms": p95_ms,
            "p99_ms": p99_ms,
            "rss_start_mb": mem_start["rss_mb"],
            "rss_end_mb": mem_end["rss_mb"],
            "mem_delta_mb": mem_end["rss_mb"] - mem_start["rss_mb"],
        }

        print(f"  Executed {total_requests} sequential mixed requests in {total_elapsed:.2f} s ({rps:.1f} req/s)")
        print(f"  Latency: Min={stats['min_ms']:.2f}ms, Avg={stats['avg_ms']:.2f}ms, Median={stats['median_ms']:.2f}ms, P95={stats['p95_ms']:.2f}ms, P99={stats['p99_ms']:.2f}ms, Max={stats['max_ms']:.2f}ms")
        print(f"  Memory: Start={mem_start['rss_mb']:.2f} MB, End={mem_end['rss_mb']:.2f} MB, Delta={stats['mem_delta_mb']:+.2f} MB")
        assert stats['mem_delta_mb'] < 100, "Potential memory leak detected (>100MB growth)"

        # 2. Parallel client processes stress (5 processes running concurrent requests)
        print("\n  [4.2] Multi-Process Concurrency Test (4 concurrent clients x 15 requests = 60 concurrent requests)...")
        concurrency = 4
        reqs_per_proc = 15

        def run_worker(worker_id):
            w_client = McpProcess([TARGET_BIN, ".", "--context=default"])
            try:
                w_client.init_handshake()
                w_client.wait_indexed()
                w_latencies = []
                for j in range(reqs_per_proc):
                    tool_name, args = queries[(worker_id + j) % len(queries)]
                    resp, ms = w_client.send_request("tools/call", {
                        "name": tool_name,
                        "arguments": args
                    })
                    assert "error" not in resp
                    w_latencies.append(ms)
                return w_latencies
            finally:
                w_client.close()

        with ThreadPoolExecutor(max_workers=concurrency) as executor:
            futures = [executor.submit(run_worker, w_id) for w_id in range(concurrency)]
            parallel_latencies = []
            for f in futures:
                parallel_latencies.extend(f.result())

        parallel_latencies.sort()
        p_avg = sum(parallel_latencies) / len(parallel_latencies)
        p_p95 = parallel_latencies[int(len(parallel_latencies) * 0.95)]
        print(f"  Concurrent requests: {len(parallel_latencies)} finished successfully. Avg={p_avg:.2f}ms, P95={p_p95:.2f}ms")
        print("  -> Stress & concurrency test: PASS")

        return stats
    finally:
        client.close()


def test_global_and_npx():
    print("\n[TEST 5] Global Command & npm shim Verification")

    cmd_name = "astrolabe.cmd" if sys.platform == "win32" else "astrolabe"
    npx_name = "npx.cmd" if sys.platform == "win32" else "npx"

    # 1. Global astrolabe --version
    ver_global = subprocess.check_output([cmd_name, "--version"], text=True, shell=(sys.platform == "win32")).strip()
    print(f"  Global 'astrolabe --version': {ver_global}")
    assert "0.17.4" in ver_global, f"Expected 0.17.4 in global version, got {ver_global}"

    # 2. npx -y @zlnap/astrolabe --version
    ver_npx = subprocess.check_output([npx_name, "-y", "@zlnap/astrolabe", "--version"], text=True, shell=(sys.platform == "win32")).strip()
    print(f"  npx 'npx -y @zlnap/astrolabe --version': {ver_npx}")
    assert "0.17.4" in ver_npx, f"Expected 0.17.4 in npx version, got {ver_npx}"

    # 3. Check binary file hash match
    import hashlib
    def get_md5(path):
        h = hashlib.md5()
        with open(path, "rb") as f:
            while chunk := f.read(65536):
                h.update(chunk)
        return h.hexdigest()

    target_md5 = get_md5(TARGET_BIN)
    cached_bin = r"C:\Users\z3437\.astrolabe\bin\0.17.4\win32-x64-msvc\astrolabe.exe"
    cached_md5 = get_md5(cached_bin) if os.path.exists(cached_bin) else None

    print(f"  target/release/astrolabe.exe MD5: {target_md5}")
    print(f"  .astrolabe cache binary MD5:     {cached_md5}")
    assert target_md5 == cached_md5, f"MD5 mismatch between built binary and npm cached binary!"

    # 4. Test MCP tools/list through global astrolabe process
    print("  Testing global 'astrolabe' MCP communication...")
    g_client = McpProcess([cmd_name, "."], cwd=PROJECT_DIR)
    try:
        g_client.init_handshake()
        g_client.wait_indexed()
        resp, _ = g_client.send_request("tools/list", {})
        tools = resp.get("result", {}).get("tools", [])
        tool_names = sorted([t["name"] for t in tools])
        assert tool_names == sorted(CANONICAL_7), f"Global astrolabe tool list mismatch: {tool_names}"

        # Test calling search_code via global astrolabe
        resp, _ = g_client.send_request("tools/call", {
            "name": "search_code",
            "arguments": {"query": "AstrolabeServer"}
        })
        assert "error" not in resp
        assert resp.get("result", {}).get("content", [{}])[0].get("text", "").startswith("index_root:")
        print("  [OK] Global astrolabe successfully responds with canonical tools and valid RPC")
    finally:
        g_client.close()

    print("  -> Global and npx shim verification: PASS")
    return {
        "ver_global": ver_global,
        "ver_npx": ver_npx,
        "md5": target_md5,
        "status": "PASS"
    }


def main():
    print("=" * 80)
    print("ASTROLABE RELEASE BINARY COMPREHENSIVE VERIFICATION SUITE")
    print(f"Target Binary: {TARGET_BIN}")
    print("=" * 80)

    t0 = time.time()
    s1 = test_schema_and_list()
    s2 = test_canonical_7_tools()
    s3 = test_aliases_compatibility()
    s4 = test_stress_and_concurrency()
    s5 = test_global_and_npx()
    total_time = time.time() - t0

    print("\n" + "=" * 80)
    print(f"ALL VERIFICATION SUITES COMPLETED SUCCESSFULLY in {total_time:.2f}s")
    print("=" * 80)

if __name__ == "__main__":
    main()
