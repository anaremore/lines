#!/usr/bin/env bash
# Runs guard.py with whichever Python works here (macOS ships python3; Windows often only
# python, and its python3 may be a Store stub). Fails closed: no working Python, or any exit
# other than 0 (allow/ask) and 2 (deny), blocks the tool call.
input=$(cat)
dir=$(cd "$(dirname "$0")" && pwd)
for py in python3 python; do
  if "$py" -c "import sys; sys.exit(0)" >/dev/null 2>&1; then
    printf '%s' "$input" | "$py" "$dir/guard.py"
    rc=$?
    if [ "$rc" -eq 0 ] || [ "$rc" -eq 2 ]; then exit "$rc"; fi
    echo "guard.py exited $rc; blocking to be safe." >&2
    exit 2
  fi
done
echo "No working Python for .claude/hooks/guard.py; blocking to be safe." >&2
exit 2
