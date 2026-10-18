#!/usr/bin/env bash
# Clone the public corpora listed in corpora.json into $BENCH_CORPORA (default ./bench-work/corpora)
# and print each checkout's tree sha next to our reference sha. Usage: bench/fetch_corpora.sh [name ...]
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
DEST=${BENCH_CORPORA:-$PWD/bench-work/corpora}
mkdir -p "$DEST"
names=("$@"); [ ${#names[@]} -gt 0 ] || mapfile -t names < <(python3 -c "import json;print('\n'.join(json.load(open('$HERE/corpora.json'))['corpora']))")
for n in "${names[@]}"; do
  url=$(python3 -c "import json;print(json.load(open('$HERE/corpora.json'))['corpora']['$n']['url'])")
  ref=$(python3 -c "import json;print(json.load(open('$HERE/corpora.json'))['corpora']['$n']['reference_tree_sha'])")
  [ -d "$DEST/$n/.git" ] || git clone --depth 1 "$url" "$DEST/$n"
  got=$(git -C "$DEST/$n" rev-parse 'HEAD^{tree}')
  if [ "$got" = "$ref" ]; then echo "$n: tree sha matches reference"; else echo "$n: tree sha $got != reference $ref (expected counts in tasks.json may not match)"; fi
done
