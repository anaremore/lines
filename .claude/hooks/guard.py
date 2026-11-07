"""PreToolUse guard for Lines (CLAUDE.md "Never" and tier C).

Reads the hook event as JSON on stdin. Exit 2 blocks the call (stderr reaches Claude); a JSON
"ask" decision pauses for the owner; exit 0 allows. Every part of a compound command is checked
and a deny anywhere wins over an ask. Run through `guard.sh`, which fails closed if Python is
missing or this script crashes. Pattern-based: a guardrail for honest mistakes, not a security
boundary. Tested by `test_guard.py` (CLAUDE.md gate step 8, and CI).
"""

import json
import os
import re
import subprocess
import sys

ASKS = []

TOKEN = re.compile(r"""(?:[^\s"']+|"[^"]*"|'[^']*')+""")
WRAPPERS = {"&", "(", "command", "env", "time", "exec", "nohup", "sudo", "call"}
WRITE_PROGS = {"rm", "del", "mv", "cp", "remove-item", "move-item", "copy-item", "set-content",
               "add-content", "out-file", "new-item", "tee", "rename-item", "rmdir", "rd"}
GUARD_IN_SHELL = re.compile(
    r"""(^|[\s/\\"'=])(\.claude[/\\](?!worktrees|projects)|\.github[/\\]|CLAUDE(\.local)?\.md)"""
)
SECRET = re.compile(r"\b(\w*_API_KEY|LINES_CQG_PASSWORD|RITHMIC_\w*PASS\w*)\s*=")


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
                "permissionDecisionReason": "; ".join(dict.fromkeys(ASKS)),
            }
        }
        print(json.dumps(out))
    sys.exit(0)


def guardrail_path(path):
    """CLAUDE.md, CLAUDE.local.md, .claude/** (not worktrees or memory) and .github/**."""
    low = path.replace("\\", "/").lower()
    # A worktree under .claude/worktrees/<name>/ is a checkout: judge the path inside it.
    low = re.sub(r"^.*?(^|/)\.claude/worktrees/[^/]+/", "/", low)
    if re.search(r"(^|/)\.claude/projects/", low):
        return False
    return bool(re.search(r"(^|/)\.claude/|(^|/)\.github/|(^|/)claude(\.local)?\.md$", low))


def native_path(p):
    """Git Bash paths (`/z/x`, `~/x`) as the native paths Python can open."""
    if not p:
        return p
    p = os.path.expanduser(p)
    m = re.match(r"^/([a-zA-Z])(/.*)?$", p)
    if os.name == "nt" and m:
        return f"{m.group(1).upper()}:{m.group(2) or '/'}"
    return p


def current_branch(cwd):
    """The checked-out branch; "" when detached; None when it can't be determined."""
    try:
        out = subprocess.run(
            ["git", "symbolic-ref", "--short", "-q", "HEAD"],
            cwd=native_path(cwd) or None, capture_output=True, text=True, timeout=5,
        )
        if out.returncode not in (0, 1):
            return None
        return out.stdout.strip()
    except Exception:
        return None


def split_parts(cmd):
    return [p.strip() for p in re.split(r"&&|\|\||[;|\n]", cmd) if p.strip()]


def tokens(part):
    return [t.replace('"', "").replace("'", "") for t in TOKEN.findall(part)]


def prog_name(tok):
    return re.sub(r"\.exe$", "", re.split(r"[\\/]", tok.strip("()"))[-1].lower())


def command_of(part):
    """The program a simple command runs: env assignments and wrappers (`&`, `command`, `(`) skipped."""
    toks = tokens(part)
    i = 0
    while i < len(toks) and (re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", toks[i]) or toks[i].lower() in WRAPPERS):
        i += 1
    if i >= len(toks):
        return "", []
    return prog_name(toks[i]), toks[i + 1:]


def short_flags(args):
    return "".join(a[1:] for a in args if re.match(r"^-[A-Za-z]+$", a))


def check_git(args, cwd, raw):
    if re.search(r"core\.hookspath", raw, re.I):
        deny("disabling git hooks")
    if re.search(r"commit\.gpgsign=false", raw, re.I):
        deny("disabling commit signing")
    i = 0
    while i < len(args) and args[i].startswith("-"):
        if args[i] == "-C" and i + 1 < len(args):
            cwd = args[i + 1]
        i += 2 if args[i] in ("-C", "-c") else 1
    sub, rest = (args[i].lower(), args[i + 1:]) if i < len(args) else ("", [])
    flags = short_flags(rest)
    joined = " ".join(rest)
    if sub == "push":
        refs = [a for a in rest if not a.startswith("-")]
        if any(re.search(r"(^|:)(refs/heads/)?main$", r) for r in refs[1:]):
            deny("pushing to main (open a PR)")
        # `git push [remote] --tags` pushes tags only (asked below), never the current branch.
        if (len(refs) <= 1 and "--tags" not in rest) or "HEAD" in refs[1:]:
            branch = current_branch(cwd)
            if branch == "main":
                deny("pushing from main (open a PR)")
            if branch is None:
                deny("pushing with no explicit branch where the current branch is unknown (name the branch)")
        if "--no-verify" in rest:
            deny("skipping push hooks")
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
    elif sub in ("tag", "update-ref"):
        if "d" in flags or "--delete" in joined:
            deny("deleting a tag or ref")
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
        if "n" in flags or "--no-verify" in rest or "--no-gpg-sign" in rest:
            deny("skipping commit hooks or signing")
    elif sub == "apply":
        ask("applying a patch (tier C if it touches the agent rules)")
    if sub in ("checkout", "restore", "rm", "mv") and GUARD_IN_SHELL.search(joined):
        ask("changing the agent rules, guardrails or CI with git (tier C)")


def check_gh(args):
    sub = " ".join(a.lower() for a in args[:2])
    joined = " ".join(args)
    if sub in ("repo delete", "release delete"):
        deny("deleting a repo or release")
    if args[:1] == ["api"] and re.search(r"(-X|--method)[=\s]*DELETE\b", joined, re.I):
        deny("DELETE through the GitHub API")
    if sub == "pr merge":
        if re.search(r"--admin\b|--delete-branch\b|(^|\s)-d(\s|$)", joined):
            deny("merging with --admin or deleting the branch")
        num = next((a for a in args[2:] if re.match(r"^(\d+|https?://\S+/pull/\d+)$", a)), "")
        try:
            out = subprocess.run(
                ["gh", "pr", "diff", *([num] if num else []), "--name-only"],
                capture_output=True, text=True, timeout=30,
            )
            files = out.stdout.splitlines() if out.returncode == 0 else None
        except Exception:
            files = None
        if files is None:
            ask("merging a PR whose changed files couldn't be listed")
        elif any(guardrail_path(f) for f in files):
            ask("merging a PR that changes the agent rules, guardrails or CI (tier C)")
    if sub in ("release create", "release edit", "release upload"):
        ask("creating or changing a GitHub release")


def check_shell(cmd, cwd):
    for part in split_parts(cmd):
        prog, args = command_of(part)
        if prog in ("cd", "pushd", "set-location", "sl", "chdir") and args:
            cwd = args[0]
            continue
        if SECRET.search(part):
            deny("a secret inline in a command")
        if "sim-once" in part and any(prog_name(t) == "cargo" for t in tokens(part)):
            deny("sending a Sierra order (the owner runs Sim tests)")
        if prog == "git":
            check_git(args, cwd, part)
        elif prog == "gh":
            check_gh(args)
        # Writes: a write program (or sed -i), or an output redirect that isn't 2>&1 / >&2.
        writes = prog in WRITE_PROGS or (prog == "sed" and any(a.startswith("-i") for a in args))
        target = " ".join(args) if writes else ""
        for m in re.finditer(r"(?<![0-9&])>>?\s*(?!&)(\"[^\"]*\"|'[^']*'|\S+)", part):
            target += " " + m.group(1)
        if not target:
            continue
        if re.search(r"sierra\s?chart", target, re.I):
            deny("modifying Sierra Chart files")
        if "disableAllHooks" in part:
            deny("disabling hooks")
        if GUARD_IN_SHELL.search(target):
            ask("changing the agent rules, guardrails or CI (tier C)")
        if re.search(r"docs[/\\]decisions[/\\]", target):
            ask("changing a decision record from the shell (the owner accepts decisions)")


def check_file(inp):
    path = inp.get("file_path") or inp.get("notebook_path") or ""
    low = path.replace("\\", "/").lower()
    text = inp.get("content") or inp.get("new_string") or ""
    text += "".join(e.get("new_string", "") for e in inp.get("edits") or [])
    if low.endswith(".json") and "disableallhooks" in text.lower():
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
                    ask("editing an accepted decision (only 'Superseded by NNNN', with the owner's yes)")
        if re.search(r"Status:\*\*\s*Accepted", text):
            ask("marking a decision Accepted (the owner accepts decisions)")
    if low.endswith("cargo.toml") and re.search(r"(default|features)\s*=\s*\[[^\]]*vendor-cqg", text):
        deny("vendor-cqg must never be on by default or through a dependency")
    if guardrail_path(low):
        ask("changing the agent rules, guardrails or CI (tier C)")


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
