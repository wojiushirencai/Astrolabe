#!/usr/bin/env python3
"""Declaration-recall gate for the languages added in v0.11 (lang8 batch).

Two corpora per language: the original small pin and a larger real repo
(Alamofire, SDWebImage, MMKV/ObjC++, kotlinx.coroutines, rubocop, nvm,
lua-language-server, element-plus); all 15 run in well under a minute.

For each pinned corpus (see scripts/fetch-corpora.sh, dest lang8/<lang>) this
starts the real MCP server over stdio, waits for the symbol index, then asks
find_symbol(exact) for every ground-truth declaration in gt/<lang>.json.gz
([file, line|null, name]). A declaration counts as found when an anchor in the
answer points at the same file and a line within [-3, +20] of the GT line
(attribute blocks above the name). Truncated answers are re-queried with
path_filter=<file>.

Gate per language: recall >= min_recall and the 200-name find_symbol sample
(every definition of the name found) >= min_fs200.  Python stdlib only.

usage: recall_gate.py BIN [lang ...]     (CORPUS_ROOT defaults to <repo>/corpus)
"""
import collections, gzip, json, os, random, re, shutil, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS_ROOT = os.environ.get("CORPUS_ROOT", os.path.join(ROOT, "corpus"))

# lang: (corpus label, min recall %, min fs200). Thresholds sit just under the
# lang8 results measured on PR #51 and on this gate (99.98-100%, fs200 = 200/200).
GATES = {
    "swift":  ("swift-argument-parser", 99.9, 200),
    "objc":   ("AFNetworking",          99.9, 200),
    "kotlin": ("okhttp",                99.9, 200),
    "ruby":   ("sinatra",               99.9, 200),
    "shell":  ("bats-core",             99.9, 200),
    "lua":    ("nvim-lspconfig",        99.9, 200),
    "vue":    ("vue-vben-admin",        99.9, 200),
    # Larger real-repo corpora (one per new language, plus ObjC++). GT is the
    # independent per-language extractor output from the lang8 bench
    # (deduplicated); thresholds sit just under the locally measured recall.
    "swift-alamofire":   ("Alamofire",          99.9, 200),
    "objc-sdwebimage":   ("SDWebImage",         99.9, 200),
    "objcpp-mmkv":       ("MMKV (.mm)",         99.9, 200),
    "kotlin-coroutines": ("kotlinx.coroutines", 99.9, 200),
    "ruby-rubocop":      ("rubocop",            99.9, 200),
    "shell-nvm":         ("nvm",                99.9, 200),
    "lua-luals":         ("lua-language-server", 99.9, 200),
    "vue-element-plus":  ("element-plus",       99.9, 200),
}


class MCP:
    def __init__(self, binary, root):
        env = dict(os.environ, ASTROLABE_SEARCH_REPEAT_MAX="0", ASTROLABE_SEARCH_RATE_MAX="0")
        self.log = open(os.path.join(os.environ.get("RUNNER_TEMP", "/tmp"), f"astro-gate-{os.path.basename(root)}.log"), "w")
        self.p = subprocess.Popen([binary, root], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=self.log, env=env, cwd=root, text=True)
        self.i = 0
        self.req("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                "clientInfo": {"name": "recall-gate", "version": "1"}})
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})

    def _send(self, msg):
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()

    def req(self, method, params):
        self.i += 1
        self._send({"jsonrpc": "2.0", "id": self.i, "method": method, "params": params})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("server exited")
            r = json.loads(line)
            if r.get("id") == self.i:
                return r

    def call(self, name, args):
        r = self.req("tools/call", {"name": name, "arguments": args})
        if "error" in r:
            return "ERROR " + json.dumps(r["error"])
        return "\n".join(c.get("text", "") for c in r["result"]["content"] if c.get("type") == "text")

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


ANCHOR = re.compile(r"(.+):(\d+)(?:\s+\[[^\]]*\])?\s*")


def anchors(text):
    out = set()
    for line in text.splitlines():
        i = line.rfind("  @")
        if i < 0:
            continue
        m = ANCHOR.fullmatch(line[i + 3:])
        if m:
            out.add((m.group(1), int(m.group(2))))
    return out


def hit(lines, l):
    return bool(lines) if l is None else any(-3 <= x - l <= 20 for x in lines)


def run(binary, lang):
    label, min_recall, min_fs = GATES[lang]
    root = os.path.join(CORPUS_ROOT, "lang8", lang)
    gt = json.load(gzip.open(os.path.join(HERE, "gt", f"{lang}.json.gz")))
    shutil.rmtree(os.path.join(root, ".astrolabe"), ignore_errors=True)
    t0 = time.time()
    m = MCP(binary, root)
    try:
        while "Index is building in the background" in m.call("get_lang", {}):
            time.sleep(0.1)
        ready = time.time() - t0
        byname = collections.defaultdict(list)
        for f, l, n in gt:
            byname[n].append((f, l))
        got = {}
        for n, defs in byname.items():
            args = {"query": n, "substring": False, "budget_tokens": 32000}
            t = m.call("find_symbol", args)
            a = anchors(t)
            if "truncated=true" in t:
                for f in {d[0] for d in defs}:
                    a |= anchors(m.call("find_symbol", dict(args, path_filter=f)))
            got[n] = a
    finally:
        m.close()
    total = time.time() - t0
    ok, miss = 0, []
    for f, l, n in gt:
        if hit([x for (ff, x) in got[n] if ff == f], l):
            ok += 1
        else:
            miss.append((f, l, n))
    names = sorted(byname)
    random.Random(1).shuffle(names)
    names = names[:200]
    fs = sum(1 for n in names if all(hit([x for (ff, x) in got[n] if ff == f], l) for f, l in byname[n]))
    recall = 100.0 * ok / len(gt)
    passed = recall >= min_recall and fs >= min(min_fs, len(names))
    return dict(lang=lang, corpus=label, gt=len(gt), found=ok, recall=round(recall, 2),
                fs200=f"{fs}/{len(names)}", ready_s=round(ready, 2), total_s=round(total, 1),
                verdict="PASS" if passed else "FAIL", min_recall=min_recall, miss=miss[:10])


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    binary = os.path.abspath(sys.argv[1])
    langs = sys.argv[2:] or list(GATES)
    results, failed = [], False
    for lang in langs:
        try:
            r = run(binary, lang)
        except Exception as e:  # missing corpus, server crash, ...
            r = dict(lang=lang, corpus=GATES[lang][0], gt=0, found=0, recall=0.0, fs200="0/0",
                     ready_s=0, total_s=0, verdict="ERROR", min_recall=GATES[lang][1], miss=[str(e)])
        print("ASTROLABE_RECALL|" + json.dumps(r, ensure_ascii=False), flush=True)
        for miss in r["miss"]:
            print("  miss:", miss)
        failed |= r["verdict"] != "PASS"
        results.append(r)
    print(f"\n{'语言':<8} | {'语料':<22} | {'声明数':>6} | {'召回率':>7} | {'门槛':>6} | {'fs200':>7} | {'就绪s':>6} | {'总耗时s':>7} | 结果")
    print("-" * 100)
    for r in results:
        print(f"{r['lang']:<8} | {r['corpus']:<22} | {r['gt']:>6} | {r['recall']:>6}% | {r['min_recall']:>5}% | {r['fs200']:>7} | {r['ready_s']:>6} | {r['total_s']:>7} | {r['verdict']}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
