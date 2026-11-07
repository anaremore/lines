"""PreToolUse guard for Lines (CLAUDE.md "Never" and "Owner only").

Reads the hook event as JSON on stdin. Exit 2 blocks the tool call (stderr reaches Claude);
printing a JSON "ask" decision pauses for the owner; exit 0 allows. Pattern-based: a guardrail
for honest mistakes, not a security boundary. Tested by `test_guard.py` (CLAUDE.md gate step 8).
"""

import json
import os
import re
import subprocess
import sys


def deny(why):
    print(f"Blocked by .claude/hooks/guard.py: {why}. See CLAUDE.md 'Never' / 'Owner only'.", file=sys.stderr)
    sys.exit(2)


def ask(why):
    out = {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "ask",
            "permissionDecisionReason": why,
        }
    }
    print(json.dumps(out))
    sys.exit(0)


# Checked per simple command (split on && || ; | and newlines), case-insensitive.
SHELL_DENY = [
    (r"\bgit\b.*\bpush\b.*(\s|:)(refs/heads/)?main(\s|$)", "pushing to main (open a PR)"),
    (r"\bgit\b.*\bpush\b.*(--force(?!-with-lease)\b|\s-f(\s|$)|\s\+[\w./-]+)", "force push (use --force-with-lease to your own branch)"),
    (r"\bgit\b.*\bpush\b.*(--delete\b|\s-d(\s|$)|\s:[\w./-]+)", "deleting a remote branch or tag"),
    (r"\bgit\s+branch\b.*((?-i:\s-D)(\s|$)|--delete\s+--force)", "force-deleting a branch"),
    (r"\bgit\s+tag\b.*(\s-d(\s|$)|--delete\b)", "deleting a tag"),
    (r"\bgit\s+clean\b.*\s-\w*x", "git clean -x deletes gitignored recordings"),
    (r"\bgit\s+add\b.*(\s-f(\s|$)|--force\b)", "git add -f can commit licensed data"),
    (r"\bgit\s+(filter-branch|filter-repo)\b", "rewriting history"),
    (r"--no-verify\b|--no-gpg-sign\b|commit\.gpgsign=false", "skipping hooks or signing"),
    (r"\bgh\s+repo\s+delete\b|\bgh\s+release\s+delete\b", "deleting a repo or release"),
    (r"\bgh\s+api\b.*(-X|--method)\s*DELETE\b", "DELETE through the GitHub API"),
    (r"\bgh\s+pr\s+merge\b.*(--admin\b|--delete-branch\b|\s-d(\s|$))", "merging with --admin or deleting the branch"),
    (r"\bsim-once\b", "sending a Sierra order (the owner runs Sim tests)"),
    (r"\b(OPTIONIDEA_API_KEY|LINES_CQG_PASSWORD|RITHMIC_\w*PASS\w*)\s*=", "a secret inline in a command"),
]
SHELL_ASK = [
    (r"\bgh\s+release\s+(create|edit|upload)\b", "creating or changing a GitHub release"),
    (r"\bgit\s+push\b.*\bv\d+\.\d+\.\d+", "pushing a release tag"),
]


def current_branch(cwd):
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            cwd=cwd or None, capture_output=True, text=True, timeout=5,
        )
        return out.stdout.strip()
    except Exception:
        return ""


def check_shell(cmd, cwd):
    for part in re.split(r"&&|\|\||[;|\n]", cmd):
        part = part.strip()
        if not part:
            continue
        for pat, why in SHELL_DENY:
            if re.search(pat, part, re.I):
                deny(why)
        if re.search(r"sierra\s?chart", part, re.I) and re.search(
            r"(rm|del|remove-item|move-item|mv|copy-item|cp|set-content|out-file|>)", part, re.I
        ):
            deny("modifying Sierra Chart files")
        # A bare `git push` (no refspec) from main pushes main.
        if re.fullmatch(r"git\s+push(\s+-u)?(\s+origin)?", part) and current_branch(cwd) == "main":
            deny("pushing to main (open a PR)")
        for pat, why in SHELL_ASK:
            if re.search(pat, part, re.I):
                ask(why)


def check_file(inp):
    path = inp.get("file_path") or inp.get("notebook_path") or ""
    low = path.replace("\\", "/").lower()
    text = inp.get("content") or inp.get("new_string") or ""
    text += "".join(e.get("new_string", "") for e in inp.get("edits") or [])
    if "/third_party/" in low:
        deny("third_party/ is a vendor submodule")
    if low.endswith("/license.md"):
        deny("LICENSE.md is owner-only")
    if re.search(r"\.(lfr|scid|depth|lseg)$", low):
        deny("market-data files are never written by agents (decision 0010)")
    if "sierrachart" in low.replace(" ", ""):
        deny("Sierra Chart files are off-limits")
    if re.search(r"/docs/decisions/\d{4}-[^/]+\.md$", low) and os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            if re.search(r"\*\*Status:\*\*\s*Accepted", f.read()):
                deny("accepted decision records are superseded by a new record, never edited")
    if low.endswith("cargo.toml") and re.search(r"default\s*=\s*\[[^\]]*vendor-cqg", text):
        deny("vendor-cqg must never be a default feature")
    if "/.claude/hooks/" in low or low.endswith("/.claude/settings.json"):
        ask("the agent guardrails themselves (tier C): owner approval")


def main():
    ev = json.load(sys.stdin)
    tool = ev.get("tool_name", "")
    inp = ev.get("tool_input") or {}
    if tool in ("Bash", "PowerShell"):
        check_shell(inp.get("command", ""), ev.get("cwd"))
    elif tool in ("Edit", "Write", "MultiEdit", "NotebookEdit"):
        check_file(inp)
    sys.exit(0)


if __name__ == "__main__":
    main()
