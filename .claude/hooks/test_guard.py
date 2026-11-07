"""Tests for guard.py: run `python .claude/hooks/test_guard.py` (CLAUDE.md gate step 8, and CI)."""

import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
GUARD = os.path.join(HERE, "guard.py")


def run(tool, tool_input, cwd=None):
    ev = {"tool_name": tool, "tool_input": tool_input, "cwd": cwd or HERE}
    p = subprocess.run([sys.executable, GUARD], input=json.dumps(ev), capture_output=True, text=True)
    if p.returncode == 2:
        return "deny"
    if p.returncode != 0:
        return f"error {p.returncode}: {p.stderr}"
    if p.stdout.strip():
        return json.loads(p.stdout)["hookSpecificOutput"]["permissionDecision"]
    return "allow"


def sh(cmd):
    return run("Bash", {"command": cmd})


def edit(path, new="x"):
    return run("Edit", {"file_path": path, "old_string": "a", "new_string": new})


CASES = [
    # Pushing and history.
    (sh("git push origin main"), "deny"),
    (sh("git push -q origin integrate:main"), "deny"),
    (sh("git push origin HEAD:refs/heads/main"), "deny"),
    (sh("git push -u origin agent-workflow"), "allow"),
    (sh("git push origin main-fixes"), "allow"),
    (sh("git push --force origin feature"), "deny"),
    (sh("git push -f origin feature"), "deny"),
    (sh("git push origin +feature"), "deny"),
    (sh("git push --force-with-lease origin feature"), "allow"),
    (sh("git push --force-with-lease origin main"), "deny"),
    (sh("git push origin --delete old"), "deny"),
    (sh("git push origin :old"), "deny"),
    (sh("git branch -D old"), "deny"),
    (sh("git branch -d merged"), "allow"),
    (sh("git tag -d v0.1.0"), "deny"),
    (sh("git clean -fdx"), "deny"),
    (sh("git clean -fd"), "allow"),
    (sh("git add -f recordings/x.lfr"), "deny"),
    (sh("git add -A"), "allow"),
    (sh("git commit --no-verify -m x"), "deny"),
    (sh("git filter-repo --path x"), "deny"),
    # Compound commands are split; an unrelated "main" elsewhere doesn't trip the push rule.
    (sh("git push -u origin feat && echo main"), "allow"),
    (sh("cargo fmt --all --check && git push origin main"), "deny"),
    # GitHub.
    (sh("gh pr merge 12 --merge"), "allow"),
    (sh("gh pr merge 12 --merge --admin"), "deny"),
    (sh("gh pr merge 12 --merge --delete-branch"), "deny"),
    (sh("gh release delete v0.1.0"), "deny"),
    (sh("gh api -X DELETE repos/a/b/git/refs/heads/x"), "deny"),
    (sh("gh release create v0.2.0 --draft"), "ask"),
    (sh("gh pr create --base main --title x --body-file b.md"), "allow"),
    # Trading, vendor files, secrets.
    (sh("cargo run -p lines-official --features sim-once"), "deny"),
    (sh('Remove-Item "C:\\SierraChart\\Data\\x.scid"'), "deny"),
    (sh('ls "C:\\SierraChart\\SierraChart - Apex\\Data"'), "allow"),
    (sh("OPTIONIDEA_API_KEY=abc cargo run"), "deny"),
    # Everyday commands stay allowed.
    (sh("~/.cargo/bin/cargo.exe test --workspace --no-fail-fast"), "allow"),
    (sh("cargo clippy --workspace --all-targets --all-features -- -D warnings"), "allow"),
    # Files.
    (edit("Z:/Projects/lines/third_party/cqg-webapi/proto/x.proto"), "deny"),
    (edit("Z:/Projects/lines/LICENSE.md"), "deny"),
    (edit("Z:/Projects/lines/recordings/x.lfr"), "deny"),
    (edit("C:/SierraChart/SierraChart - Apex/Sierra.config"), "deny"),
    (edit("Z:/Projects/lines/crates/lines-app/Cargo.toml", 'default = ["vendor-cqg"]'), "deny"),
    (edit("Z:/Projects/lines/crates/lines-app/Cargo.toml", 'vendor-cqg = ["lines-feed/vendor-cqg"]'), "allow"),
    (edit("Z:/Projects/lines/.github/workflows/ci.yml"), "allow"),
    (edit("Z:/Projects/lines/deny.toml"), "allow"),
    (edit("Z:/Projects/lines/.claude/skills/full-send/SKILL.md"), "allow"),
    (edit("Z:/Projects/lines/.claude/settings.json"), "ask"),
    (edit("Z:/Projects/lines/.claude/hooks/guard.py"), "ask"),
    (edit("Z:/Projects/lines/crates/lines-app/src/chart_view.rs"), "allow"),
    (edit("Z:/Projects/lines/CLAUDE.md"), "allow"),
    (run("Write", {"file_path": "Z:/Projects/lines/docs/invariants.md", "content": "x"}), "allow"),
]


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
            (edit(accepted), "deny"),
            (edit(proposed), "allow"),
            (edit(os.path.join(dec, "0100-new.md")), "allow"),
        ]


def main():
    cases = CASES + decision_cases()
    failed = [(i, got, want) for i, (got, want) in enumerate(cases) if got != want]
    for i, got, want in failed:
        print(f"case {i}: got {got!r}, want {want!r}")
    print(f"guard tests: {len(cases) - len(failed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
