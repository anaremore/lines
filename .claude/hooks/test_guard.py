"""Tests for the guard hook: `python .claude/hooks/test_guard.py` (CLAUDE.md gate step 8, and CI).

Runs `guard.sh` exactly as `.claude/settings.json` does (bash), so the launcher is tested too.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
GUARD_SH = os.path.join(HERE, "guard.sh")
def find_bash():
    """Git Bash on Windows (system32 bash.exe is WSL and can't see this checkout), else PATH."""
    if os.name == "nt":
        for c in (r"C:\Program Files\Git\bin\bash.exe", r"C:\Program Files\Git\usr\bin\bash.exe"):
            if os.path.exists(c):
                return c
    return shutil.which("bash")


BASH = find_bash()


def run(tool, tool_input, cwd=None):
    ev = {"tool_name": tool, "tool_input": tool_input, "cwd": cwd or HERE}
    p = subprocess.run([BASH, GUARD_SH], input=json.dumps(ev), capture_output=True, text=True, cwd=cwd or HERE)
    if p.returncode == 2:
        return "deny"
    if p.returncode != 0:
        return f"error {p.returncode}: {p.stderr}"
    if p.stdout.strip():
        return json.loads(p.stdout)["hookSpecificOutput"]["permissionDecision"]
    return "allow"


def sh(cmd, cwd=None):
    return run("Bash", {"command": cmd}, cwd)


def edit(path, new="x"):
    return run("Edit", {"file_path": path, "old_string": "a", "new_string": new})


R = "Z:/Projects/lines/"

CASES = [
    # Pushing.
    ("git push origin main", "deny"),
    ('git push origin "main"', "deny"),
    ("git push -q origin integrate:main", "deny"),
    ("git push origin HEAD:refs/heads/main", "deny"),
    ("git push -u origin agent-workflow", "allow"),
    ("git push origin main-fixes", "allow"),
    ("git push --force origin feature", "deny"),
    ("git push -f origin feature", "deny"),
    ("git push -uf origin feature", "deny"),
    ("git push origin +feature", "deny"),
    ("git push --force-with-lease origin feature", "allow"),
    ("git push --force-with-lease --force-if-includes origin feature", "allow"),
    ("git push --force-with-lease origin main", "deny"),
    ("git push origin --delete old", "deny"),
    ("git push origin :old", "deny"),
    ("git push --mirror origin", "deny"),
    ("git push --prune origin", "deny"),
    ("git push --tags", "ask"),
    ("git push origin v0.2.0", "ask"),
    # Branches, tags, history, hooks.
    ("git branch -D old", "deny"),
    ("git branch -d -f old", "deny"),
    ("git branch -d merged", "allow"),
    ("git tag -d v0.1.0", "deny"),
    ("git update-ref -d refs/heads/x", "deny"),
    ("git clean -fdx", "deny"),
    ("git clean -fd", "allow"),
    ("git add -f recordings/x.lfr", "deny"),
    ("git add -Af", "deny"),
    ("git add -A", "allow"),
    ("git commit --no-verify -m x", "deny"),
    ("git commit -n -m x", "deny"),
    ("git -c core.hooksPath=/dev/null commit -m x", "deny"),
    ('git commit -m "push fix to main branch"', "allow"),
    ("git filter-repo --path x", "deny"),
    ("git reflog expire --expire=now --all", "deny"),
    # Compound commands: every part is checked, and a deny anywhere wins over an ask.
    ("git push -u origin feat && echo main", "allow"),
    ("cargo fmt --all --check && git push origin main", "deny"),
    ("gh release create v0.2.0 && git push origin main", "deny"),
    # GitHub.
    ("gh pr merge 999999 --merge", "ask"),  # changed files can't be listed: ask
    ("gh pr merge 12 --merge --admin", "deny"),
    ("gh pr merge 12 --merge --delete-branch", "deny"),
    ("gh release delete v0.1.0", "deny"),
    ("gh api -X DELETE repos/a/b/git/refs/heads/x", "deny"),
    ("gh api --method=DELETE repos/a/b/git/refs/heads/x", "deny"),
    ("gh release create v0.2.0 --draft", "ask"),
    ("gh pr create --base main --title x --body-file b.md", "allow"),
    # Wrappers and quoting don't hide the program.
    ('LINES_SIERRA_DATA="C:/SierraChart/SierraChart - Apex/Data" cargo test -p lines-official --features sim-once', "deny"),
    ('& "C:/Users/human/.cargo/bin/cargo.exe" run -p lines-official --features sim-once', "deny"),
    ("(cargo run -p lines-official --features sim-once)", "deny"),
    ("& git push origin main", "deny"),
    ("command git push origin main", "deny"),
    ('git commit -m "guard: deny disableAllHooks"', "allow"),
    ('git commit -m "guard: block --no-verify"', "allow"),
    ("git push --no-verify origin feature", "deny"),
    # Searching for rule words is fine.
    ('rg -n -- "--no-verify" CLAUDE.md', "allow"),
    ("grep -rn sim-once crates", "allow"),
    # Trading, Sierra, secrets.
    ("cargo run -p lines-official --features sim-once", "deny"),
    ('Remove-Item "C:\\SierraChart\\Data\\x.scid"', "deny"),
    ('Set-Content "C:\\SierraChart\\Sierra.config" x', "deny"),
    ('echo x > "C:\\SierraChart\\Data\\x.txt"', "deny"),
    ('ls "C:\\SierraChart\\SierraChart - Apex\\Data"', "allow"),
    ('ls "C:\\SierraChart\\Data" > files.txt', "allow"),
    ('LINES_SIERRA_DATA="C:\\SierraChart\\SierraChart - Apex\\Data" cargo test -p lines-sierra -- --ignored 2>&1', "allow"),
    ("OPTIONIDEA_API_KEY=abc cargo run", "deny"),
    # The guardrails themselves.
    ("rm .claude/hooks/guard.py", "ask"),
    ("sed -i s/deny/pass/ .claude/hooks/guard.py", "ask"),
    ("git checkout x -- .claude/settings.json", "ask"),
    ("echo x > CLAUDE.md", "ask"),
    ("sed -i s/x/y/ .github/workflows/ci.yml", "ask"),
    ("cat > .github/workflows/ci.yml", "ask"),
    ("Set-Content .github/workflows/ci.yml x", "ask"),
    ("git checkout origin/x -- .github/workflows/ci.yml", "ask"),
    ("git apply fix.patch", "ask"),
    ("sed -i s/Proposed/Accepted/ docs/decisions/0021-settings.md", "ask"),
    ("cp out.png Z:/Projects/lines/.claude/worktrees/feat/assets/x.png", "allow"),
    ("cp note.md C:/Users/human/.claude/projects/z--Projects-lines/memory/x.md", "allow"),
    ('echo \'{"disableAllHooks": true}\' > .claude/settings.local.json', "deny"),
    # Everyday commands.
    ("~/.cargo/bin/cargo.exe test --workspace --no-fail-fast", "allow"),
    ("cargo clippy --workspace --all-targets --all-features -- -D warnings", "allow"),
    ("cat .claude/hooks/guard.py", "allow"),
]

FILES = [
    (R + "third_party/cqg-webapi/proto/x.proto", "x", "deny"),
    (R + "LICENSE.md", "x", "deny"),
    (R + "recordings/x.lfr", "x", "deny"),
    ("C:/SierraChart/SierraChart - Apex/Sierra.config", "x", "deny"),
    (R + "crates/lines-app/Cargo.toml", 'default = ["vendor-cqg"]', "deny"),
    (R + "crates/lines-app/Cargo.toml", 'lines-feed = { path = "../lines-feed", features = ["vendor-cqg"] }', "deny"),
    (R + "crates/lines-app/Cargo.toml", 'vendor-cqg = ["lines-feed/vendor-cqg"]', "allow"),
    (R + ".claude/settings.local.json", '{"disableAllHooks": true}', "deny"),
    (R + ".claude/settings.local.json", "{}", "ask"),
    (R + ".claude/settings.json", "{}", "ask"),
    (R + ".claude/hooks/guard.py", "x", "ask"),
    (R + ".claude/skills/full-send/SKILL.md", "x", "ask"),
    (R + ".claude/agents/reviewer.md", "x", "ask"),
    (R + ".claude/commands/x.md", "x", "ask"),
    (R + "CLAUDE.local.md", "x", "ask"),
    (R + ".claude/hooks/guard.py", 'if "disableAllHooks" in part:', "ask"),
    (R + ".claude/worktrees/feat/crates/lines-app/src/lib.rs", "x", "allow"),
    (R + ".claude/worktrees/feat/.claude/hooks/guard.py", "x", "ask"),
    (R + ".claude/worktrees/feat/.claude/settings.json", "{}", "ask"),
    (R + ".claude/worktrees/feat/CLAUDE.md", "x", "ask"),
    (R + ".claude/worktrees/feat/.github/workflows/ci.yml", "x", "ask"),
    (R + "CLAUDE.md", "x", "ask"),
    (R + ".github/workflows/ci.yml", "x", "ask"),
    (R + "deny.toml", "x", "allow"),
    (R + "crates/lines-app/src/chart_view.rs", "x", "allow"),
    (R + "docs/invariants.md", "x", "allow"),
    ("C:/Users/human/.claude/projects/z--Projects-lines/memory/x.md", "x", "allow"),
]


def main_branch_cases():
    """Pushing with no destination from a checkout on main."""
    with tempfile.TemporaryDirectory() as d:
        subprocess.run(["git", "init", "-q", "-b", "main", d], check=True)
        return [
            (sh("git push", d), "deny", "bare push on main"),
            (sh("git push origin HEAD", d), "deny", "push HEAD on main"),
            (sh("git push -u origin feature", d), "allow", "named branch from main checkout"),
            (sh(f"git -C {d} push origin HEAD"), "deny", "git -C into a checkout on main"),
            (sh(f"cd {d} && git push"), "deny", "cd into a checkout on main"),
            (sh(f"cd {bash_path(d)} && git push"), "deny", "cd with a Git Bash path"),
            (sh(f"git -C {bash_path(d)} push"), "deny", "git -C with a Git Bash path"),
            (sh(f"Set-Location {d}; git push"), "deny", "Set-Location into a checkout on main"),
            (sh("cd /no/such/dir && git push"), "deny", "unknown branch, no explicit refspec"),
            (sh("cd /no/such/dir && git push -u origin feature"), "allow", "unknown branch, named branch"),
        ]


def bash_path(p):
    """C:\\x\\y -> /c/x/y (how Git Bash spells native paths)."""
    p = p.replace("\\", "/")
    return f"/{p[0].lower()}{p[2:]}" if os.name == "nt" and p[1:2] == ":" else p


def decision_cases():
    with tempfile.TemporaryDirectory() as d:
        dec = os.path.join(d, "docs", "decisions")
        os.makedirs(dec)
        accepted = os.path.join(dec, "0099-accepted.md")
        proposed = os.path.join(dec, "0098-proposed.md")
        with open(accepted, "w", encoding="utf-8") as f:
            f.write("# 0099\n\n**Status:** Accepted\n")
        with open(proposed, "w", encoding="utf-8") as f:
            f.write("# 0098\n\n**Status:** Proposed\n")
        return [
            (edit(accepted), "ask", "edit accepted (supersede marker only, with the owner's yes)"),
            (edit(proposed), "allow", "edit proposed"),
            (edit(proposed, "**Status:** Accepted"), "ask", "accept a proposal"),
            (edit(os.path.join(dec, "0100-new.md"), "**Status:** Proposed"), "allow", "new proposal"),
        ]


def main():
    if not BASH:
        print("bash not found: the guard runs through guard.sh")
        sys.exit(1)
    results = [(sh(c), want, c) for c, want in CASES]
    results += [(edit(p, new), want, f"edit {p} <- {new}") for p, new, want in FILES]
    results += main_branch_cases() + decision_cases()
    failed = [(label, got, want) for got, want, label in results if got != want]
    for label, got, want in failed:
        print(f"FAIL {label!r}: got {got!r}, want {want!r}")
    print(f"guard tests: {len(results) - len(failed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
