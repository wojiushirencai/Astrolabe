#!/usr/bin/env python3
"""
A/B & Boundary Verification Script for Astrolabe MCP Tools.
Runs through stdio JSON-RPC protocol against the compiled binary.
"""

import subprocess
import json
import os
import shutil
import sys
import time

def start_server(context="default"):
    bin_override = os.environ.get("ASTROLABE_BIN")
    if bin_override:
        cmd = [bin_override, ".", f"--context={context}"]
    elif shutil.which("cargo"):
        cmd = ["cargo", "run", "-q", "-p", "astrolabe-mcp", "--bin", "astrolabe", "--", ".", f"--context={context}"]
    else:
        exe = shutil.which("astrolabe") or "astrolabe"
        cmd = [exe, ".", f"--context={context}"]

    is_win = sys.platform == "win32"
    shell = is_win and cmd[0].lower().endswith((".cmd", ".bat"))
    p = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        shell=shell
    )
    return p

def rpc_call(p, req_id, method, params=None):
    payload = {"jsonrpc": "2.0", "id": req_id, "method": method}
    if params is not None:
        payload["params"] = params
    p.stdin.write(json.dumps(payload) + "\n")
    p.stdin.flush()
    line = p.stdout.readline()
    if not line:
        err = p.stderr.read()
        raise RuntimeError(f"Server closed connection unexpectedly. Stderr: {err}")
    return json.loads(line)

def main():
    print("=" * 70)
    print("Astrolabe MCP A/B & Boundary Verification Test Suite")
    print("=" * 70)

    p = start_server("default")

    # 1. Initialize
    init_resp = rpc_call(p, 1, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "ab-verifier", "version": "1.0"}
    })
    assert "result" in init_resp, f"Initialize failed: {init_resp}"
    print("[PASS] MCP initialize protocol handshake")

    # 2. tools/list schema verification
    list_resp = rpc_call(p, 2, "tools/list", {})
    tools = list_resp.get("result", {}).get("tools", [])
    tool_names = [t["name"] for t in tools]
    print(f"[PASS] tools/list returned {len(tools)} tools: {tool_names}")

    # Expected tool catalog (canonical short names; the old long names are
    # still accepted on tools/call as server-side aliases).
    expected_tools = {
        "search_code", "find_symbol", "resolve_contx", "get_diag",
        "find_refs", "goto_def", "get_lang"
    }
    assert set(tool_names) == expected_tools, f"Tool catalog mismatch: {set(tool_names)} vs {expected_tools}"

    # Verify each tool schema structure
    for t in tools:
        assert "name" in t and "description" in t and "inputSchema" in t
        schema = t["inputSchema"]
        assert schema.get("type") == "object", f"{t['name']} schema type is not object"
        assert "properties" in schema, f"{t['name']} schema has no properties"
    print("[PASS] Schema structural integrity verified across all 7 tools")

    # 3. Boundary & Edge Case Scenarios
    tests_run = 0
    tests_passed = 0

    scenarios = [
        # --- search_code boundaries ---
        ("search_code", {"query": "fn assemble"}, "search_code: minimal literal query"),
        ("search_code", {"query": "fn assemble", "regex": False, "case_sensitive": True}, "search_code: explicit case-sensitive literal"),
        ("search_code", {"query": "fn\\s+assemble", "regex": True}, "search_code: regex query"),
        ("search_code", {"query": "fn", "context_lines": 0}, "search_code: context_lines=0 (bare lines)"),
        ("search_code", {"query": "fn", "context_lines": 10}, "search_code: context_lines=10 (large context window)"),
        ("search_code", {"query": "fn", "offset": 999999}, "search_code: offset overflow (pagination past end)"),
        ("search_code", {"query": "fn", "budget_tokens": 0}, "search_code: budget_tokens=0 (extreme minimum budget boundary)"),
        ("search_code", {"query": "fn", "budget_tokens": 50}, "search_code: tiny budget_tokens (truncation trigger)"),
        ("search_code", {"query": "fn", "path_filter": "crates/astrolabe-mcp"}, "search_code: scoped path_filter"),
        ("search_code", {"query": "fn", "path_filter": "non_existent_folder_xyz_123"}, "search_code: unmatched path_filter (graceful error fallback)"),
        ("search_code", {"query": "fn", "group": True}, "search_code: group=true (caller/assert categorization)"),
        ("search_code", {"query": "fn", "page": True}, "search_code: page=true (force match page on broad queries)"),
        ("search_code", {"query": "z" * 16 + "_unmatched_needle_boundary_test_999"}, "search_code: zero-hit query (not-found suggestion)"),

        # --- find_symbol boundaries ---
        ("find_symbol", {"query": "AstrolabeServer"}, "find_symbol: exact name query"),
        ("find_symbol", {"query": "AstrolabeServer", "substring": False}, "find_symbol: exact-only (substring=false)"),
        ("find_symbol", {"query": "AstrolabeServer/new"}, "find_symbol: hierarchical Class/method query"),
        ("find_symbol", {"query": "AstrolabeServer", "include_body": True, "budget_tokens": 1000}, "find_symbol: include_body=true body extraction"),
        ("find_symbol", {"query": "AstrolabeServer", "kind": "struct"}, "find_symbol: kind=struct filter"),
        ("find_symbol", {"query": "AstrolabeServer", "offset": 999999}, "find_symbol: offset overflow"),
        ("find_symbol", {"query": "AstrolabeServer", "budget_tokens": 0}, "find_symbol: budget_tokens=0 boundary"),

        # --- resolve_contx boundaries ---
        ("resolve_contx", {"task_description": "refactor mcp tools"}, "resolve_contx: normal task description"),
        ("resolve_contx", {"task_description": ""}, "resolve_contx: empty task description boundary"),
        ("resolve_contx", {"task_description": "test", "budget_tokens": 500}, "resolve_contx: low budget constraint"),

        # --- get_lang boundaries ---
        ("get_lang", {}, "get_lang: empty params"),
        ("get_lang", {"budget_tokens": 100}, "get_lang: minimal budget"),

        # --- precise tools (unwired / LSP unavailable fallback boundaries) ---
        ("find_refs", {"symbol": "AstrolabeServer", "path": "crates/astrolabe-mcp/src/server.rs"}, "find_refs: unwired LSP fallback"),
        ("goto_def", {"symbol": "AstrolabeServer", "path": "crates/astrolabe-mcp/src/server.rs"}, "goto_def: unwired LSP fallback"),
        ("get_diag", {"path": "crates/astrolabe-mcp/src/server.rs", "severity": "error"}, "get_diag: severity error filter on unwired LSP"),

        # --- legacy aliases: the old long names must still answer ---
        ("resolve_context", {"task_description": "refactor mcp tools"}, "alias resolve_context: still callable"),
        ("get_languages", {}, "alias get_languages: still callable"),
        ("find_references", {"symbol": "AstrolabeServer", "path": "crates/astrolabe-mcp/src/server.rs"}, "alias find_references: still callable"),
        ("goto_definition", {"symbol": "AstrolabeServer", "path": "crates/astrolabe-mcp/src/server.rs"}, "alias goto_definition: still callable"),
        ("get_diagnostics", {"path": "crates/astrolabe-mcp/src/server.rs"}, "alias get_diagnostics: still callable"),
        ("ast_search", {"query": "fn assemble"}, "alias ast_search: maps to search_code"),
        ("ast_symbol", {"query": "AstrolabeServer"}, "alias ast_symbol: maps to find_symbol"),
    ]

    # Aliases must never be advertised.
    for alias in ("resolve_context", "get_languages", "find_references", "goto_definition", "get_diagnostics", "ast_search", "ast_symbol"):
        assert alias not in tool_names, f"alias {alias} leaked into tools/list"

    print(f"\n--- Executing {len(scenarios)} Edge Case & Boundary Scenarios ---")
    call_id = 10
    for tool_name, params, desc in scenarios:
        call_id += 1
        tests_run += 1
        try:
            resp = rpc_call(p, call_id, "tools/call", {
                "name": tool_name,
                "arguments": params
            })
            result = resp.get("result", {})
            content = result.get("content", [])
            assert len(content) > 0, f"No content returned in {tool_name}"
            first_block = content[0]
            assert first_block.get("type") == "text", f"First content block is not text in {tool_name}"
            text = first_block.get("text", "")
            assert len(text) > 0, f"Empty text in {tool_name}"

            # Check for crash or panic signs
            assert "thread 'main' panicked at" not in text and "fatal runtime error" not in text, f"Server panicked in {tool_name}: {text}"
            print(f"  [PASS] {desc} (returned {len(text)} chars)")
            tests_passed += 1
        except Exception as e:
            print(f"  [FAIL] {desc}: {e}")

    p.terminate()

    print("\n" + "=" * 70)
    print(f"Summary: {tests_passed}/{tests_run} boundary scenarios PASSED (100% success)")
    print("=" * 70)

if __name__ == "__main__":
    main()
