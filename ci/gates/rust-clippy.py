"""Fail when Clippy reports anything, warnings denied.

Clippy compiles the workspace, so this is the slow gate. It operates on the
whole workspace, so it checks everything regardless of which files are staged.
If a crate is ever too expensive to compile on every commit (a GUI shell with
heavy system libraries, say), add `--exclude <package>` here.

An application commits its lock file, and `--locked` holds the build to it,
so a manifest out of step with it fails here rather than resolving something
new. Inside the superrepo the patch table rewrites the lock file on every
build, so there the flag is left off; see `the_test_cabinet_ci.lockfile`.
"""

from the_test_cabinet_ci import enter_repo_root, fail, succeeded
from the_test_cabinet_ci.lockfile import locked_arguments

# From the workspace root, so cargo resolves the workspace regardless of the
# caller's working directory.
root = enter_repo_root()

clippy = ["cargo", "clippy", *locked_arguments(root), "--workspace", "--all-targets"]
if not succeeded([*clippy, "--", "-D", "warnings"]):
    fail(
        "",
        "clippy found issues. Fix them, then commit again.",
        "(If a commit is genuinely fine, 'git commit --no-verify' bypasses the hook.)",
    )
