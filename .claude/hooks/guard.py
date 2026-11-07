"""PreToolUse guard for Lines (CLAUDE.md "Never" and tier C).

Reads the hook event as JSON on stdin. Exit 2 blocks the call (stderr reaches Claude); a JSON
"ask" decision pauses for the owner; exit 0 allows. Run through `guard.sh`, which fails closed
if Python is missing or this script crashes. Pattern-based: a guardrail for honest mistakes,
not a security boundary. Tested by `test_guard.py` (CLAUDE.md gate step 8, and CI).
"""

import json
import os
import re
import subprocess
import sys

ASKS = []


def deny(why):
    print(f"Blocked by .claude/hooks/guard.py: {why}. See CLAUDE.md 'Never' / tier C.", file=sys.stderr)
    sys.exit(2)


def ask(why):
    ASKS.append(why)


def finish():
    if ASKS:
        out = {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "ask",
                "permissionDecisionReason": "; ".join(ASKS),
            }
        }
        print(json.dumps(out))
    sys.exit(0)


def current_branch(cwd):
    try:
        out = subprocess.run(
            ["git", "symbolic-ref", "--short", "-q", "HEAD"],
            cwd=cwd or None, capture_output=True, text=True, timeout=5,
        )
        return out.stdout.strip()
    except Exception:
        return ""


def split_parts(cmd):
    return [p.strip() for p in re.split(r"&&|\|\||[;|\n]", cmd) if p.strip()]


def command_of(part):
    """The program a simple command runs (env assignments skipped, path and .exe dropped)."""
    toks = part.replace('"', " ").replace("'", " ").split()
    i = 0
    while i < len(toks) and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", toks[i]):
        i += 1
    if i >= len(toks):
        return "", []
    prog = re.split(r"[\\/]", toks[i])[-1].lower()
    prog = re.sub(r"\.exe$", "", prog)
    return prog, toks[i + 1:]


def git_sub(args):
    """`git -C x -c y status` -> ("status", rest)."""
    i = 0
    while i < len(args) and args[i].startswith("-"):
        i += 2 if args[i] in ("-C", "-c") else 1
    return (args[i].lower(), args[i + 1:]) if i < len(args) else ("", [])


def short_flags(args):
    return "".join(a[1:] for a in args if re.match(r"^-[A-Za-z]+$", a))


WRITE_PROGS = {"rm", "del", "mv", "cp", "remove-item", "move-item", "copy-item", "set-content",
               "add-content", "out-file", "new-item", "tee", "rename-item", "rmdir", "rd"}


def check_git(args, cwd, raw):
    if re.search(r"core\.hookspath", raw, re.I):
        deny("disabling git hooks")
    if re.search(r"commit\.gpgsign=false", raw, re.I):
        deny("disabling commit signing")
    sub, rest = git_sub(args)
    flags = short_flags(rest)
    joined = " ".join(rest)
    if sub == "push":
        refs = [a for a in rest if not a.startswith("-")]
        if any(re.search(r"(^|:)(refs/heads/)?main$", r) for r in refs[1:]):
            deny("pushing to main (open a PR)")
        if current_branch(cwd) == "main" and (len(refs) <= 1 or any(r == "HEAD" for r in refs[1:])):
            deny("pushing from main (open a PR)")
        if re.search(r"--force(?![-\w])", joined) or "f" in flags or any(r.startswith("+") for r in refs):
            deny("force push (use --force-with-lease to your own branch)")
        if re.search(r"--(delete|mirror|prune)\b", joined) or "d" in flags or any(r.startswith(":") for r in refs):
            deny("deleting or mirroring remote refs")
        if re.search(r"--(tags|follow-tags)\b", joined) or any(
            re.search(r"(^|:)(refs/tags/)?v\d+\.\d+", r) for r in refs[1:]
        ):
            ask("pushing a release tag (release.yml builds a pre-release)")
    elif sub == "branch":
        if "D" in flags or ("d" in flags and "f" in flags) or (
            "--delete" in joined and re.search(r"--force|(^|\s)-f(\s|$)", joined)
        ):
            deny("force-deleting a branch")
    elif sub == "tag":
        if "d" in flags or "--delete" in joined:
            deny("deleting a tag")
    elif sub == "update-ref":
        if "d" in flags or "--delete" in joined:
            deny("deleting a ref")
    elif sub == "clean":
        if "x" in flags.lower():
            deny("git clean -x deletes gitignored recordings")
    elif sub == "add":
        if "f" in flags or "--force" in joined:
            deny("git add -f can commit licensed data")
    elif sub in ("filter-branch", "filter-repo"):
        deny("rewriting history")
    elif sub == "reflog" and rest[:1] == ["expire"]:
        deny("expiring the reflog")
    elif sub == "commit":
        if "n" in flags or "--no-verify" in joined or "--no-gpg-sign" in joined:
            deny("skipping commit hooks or signing")
    if sub in ("checkout", "restore", "rm", "mv") and re.search(r"(^|[\s/\\])\.claude[/\\]|CLAUDE\.md", joined):
        ask("changing the agent guardrails (tier C)")


def check_gh(args):
    sub = " ".join(a.lower() for a in args[:2])
    joined = " ".join(args)
    if sub in ("repo delete", "release delete"):
        deny("deleting a repo or release")
    if args[:1] == ["api"] and re.search(r"(-X|--method)[=\s]*DELETE\b", joined, re.I):
        deny("DELETE through the GitHub API")
    if sub == "pr merge" and re.search(r"--admin\b|--delete-branch\b|(^|\s)-d(\s|$)", joined):
        deny("merging with --admin or deleting the branch")
    if sub in ("release create", "release edit", "release upload"):
        ask("creating or changing a GitHub release")


def check_shell(cmd, cwd):
    if re.search(r"disableAllHooks", cmd):
        deny("disabling hooks")
    for part in split_parts(cmd):
        prog, args = command_of(part)
        if re.search(r"\b(OPTIONIDEA_API_KEY|LINES_CQG_PASSWORD|RITHMIC_\w*PASS\w*)\s*=", part):
            deny("a secret inline in a command")
        if prog == "git":
            check_git(args, cwd, part)
        elif prog == "gh":
            check_gh(args)
        elif prog == "cargo" and re.search(r"\bsim-once\b", part):
            deny("sending a Sierra order (the owner runs Sim tests)")
        # Writes: a write program (or sed -i), or an output redirect that isn't 2>&1.
        writes = prog in WRITE_PROGS or (prog == "sed" and any(a.startswith("-i") for a in args))
        target = " ".join(args) if writes else ""
        redirect = re.search(r"(?<![0-9&])>>?\s*(?!&)(\S+)", part)
        if redirect:
            target += " " + redirect.group(1)
        if re.search(r"sierra\s?chart", target, re.I):
            deny("modifying Sierra Chart files")
        if re.search(r"(^|[\s/\\\"'])\.claude[/\\]|CLAUDE\.md", target):
            ask("changing the agent guardrails (tier C)")


def check_file(inp):
    path = inp.get("file_path") or inp.get("notebook_path") or ""
    low = path.replace("\\", "/").lower()
    text = inp.get("content") or inp.get("new_string") or ""
    text += "".join(e.get("new_string", "") for e in inp.get("edits") or [])
    if "disableallhooks" in text.lower():
        deny("disabling hooks")
    if "/third_party/" in low:
        deny("third_party/ is a vendor submodule")
    if low.endswith("/license.md"):
        deny("LICENSE.md is owner-only")
    if re.search(r"\.(lfr|scid|depth|lseg)$", low):
        deny("market-data files are never written by agents (decision 0010)")
    if "sierrachart" in low.replace(" ", ""):
        deny("Sierra Chart files are off-limits")
    if re.search(r"/docs/decisions/\d{4}-[^/]+\.md$", low):
        if os.path.exists(path):
            with open(path, encoding="utf-8") as f:
                if re.search(r"\*\*Status:\*\*\s*Accepted", f.read()):
                    deny("accepted decision records are superseded by a new record, never edited")
        if re.search(r"Status:\*\*\s*Accepted", text):
            ask("marking a decision Accepted (the owner accepts decisions)")
    if low.endswith("cargo.toml") and re.search(r"(default|features)\s*=\s*\[[^\]]*vendor-cqg", text):
        deny("vendor-cqg must never be on by default or through a dependency")
    if re.search(r"/\.claude/(settings[^/]*\.json|hooks/|skills/|agents/)", low) or low.endswith("/claude.md") \
            or "/.github/" in low:
        ask("changing the agent guardrails, rules or CI (tier C)")


def main():
    ev = json.load(sys.stdin)
    tool = ev.get("tool_name", "")
    inp = ev.get("tool_input") or {}
    if tool in ("Bash", "PowerShell"):
        check_shell(inp.get("command", ""), ev.get("cwd"))
    elif tool in ("Edit", "Write", "MultiEdit", "NotebookEdit"):
        check_file(inp)
    finish()


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as e:  # fail closed
        print(f"guard.py crashed ({type(e).__name__}: {e}); blocking to be safe.", file=sys.stderr)
        sys.exit(2)
