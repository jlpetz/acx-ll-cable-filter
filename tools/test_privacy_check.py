"""Tests for tools/privacy_check.py on a throwaway repository: what a push or --all must refuse (Astra's recheck,
2026-11-23: a file's path, a commit's display name, an annotated tag's tagger and message; Astra's review, 2026-11-24: a
branch's or lightweight tag's name, a path whose contents another path also holds, and a tree or file a tag names, its paths
and contents), and a clean one passing.

The private-looking strings are built at run time, so this file never holds one (the check reads it too).

    python -I tools/test_privacy_check.py
"""
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECK = Path(__file__).resolve().parent / "privacy_check.py"
NOREPLY = "someone" + "@users.noreply.github.com"
# An EC2-style internal host name and a plain email address, assembled so this file doesn't match.
HOST = "ip-" + "-".join(["10", "1", "2", "3"])
EMAIL = "someone" + "@" + "example.com"
DENIED = "zq" + "secretname"
ZERO = "0" * 40


def git(repo, *args, env=None, stdin=None):
    r = subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True, env=env, input=stdin)
    if r.returncode != 0:
        raise AssertionError(f"git {' '.join(args)}: {r.stderr}")
    return r.stdout.strip()


class PrivacyCheck(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = self.tmp.name
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "config", "user.email", NOREPLY)
        git(self.repo, "config", "user.name", "Someone")
        git(self.repo, "config", "commit.gpgsign", "false")
        git(self.repo, "config", "tag.gpgsign", "false")
        info = Path(self.repo, ".git", "info")
        info.mkdir(parents=True, exist_ok=True)
        (info / "privacy-deny").write_text(DENIED + "\n", encoding="utf-8")
        self.commit("README.md", "clean\n", "a clean start")

    def tearDown(self):
        self.tmp.cleanup()

    def commit(self, path, text, message, name="Someone"):
        p = Path(self.repo, path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")
        git(self.repo, "add", "--", path)
        env = dict(os.environ, GIT_AUTHOR_NAME=name, GIT_COMMITTER_NAME=name)
        git(self.repo, "commit", "-q", "-m", message, env=env)
        return git(self.repo, "rev-parse", "HEAD")

    def check(self, *args, stdin=None):
        r = subprocess.run([sys.executable, "-I", str(CHECK), *args], cwd=self.repo, capture_output=True, text=True, input=stdin)
        return r.returncode, r.stdout

    def push(self, sha, ref="refs/heads/main", base=ZERO, remote_ref=None):
        return self.check("--pre-push", "origin", "url", stdin=f"{ref} {sha} {remote_ref or ref} {base}\n")

    def test_clean_history_passes(self):
        self.assertEqual(self.check("--all"), (0, "privacy check: clean\n"))

    def tree(self, entries):
        """A tree object from (name, text) pairs, a text a file and a list a folder; no commit holds it. mktree -z: a text
        stdin on Windows writes a newline as CR LF, and mktree would keep the CR in the name."""
        lines = []
        for name, value in entries:
            if isinstance(value, list):
                lines.append(f"040000 tree {self.tree(value)}\t{name}\0")
            else:
                lines.append(f"100644 blob {git(self.repo, 'hash-object', '-w', '--stdin', stdin=value)}\t{name}\0")
        return git(self.repo, "mktree", "-z", stdin="".join(lines))

    def test_a_path_is_checked(self):
        sha = self.commit(f"logs/{HOST}.txt", "nothing here\n", "a log")
        changed = self.commit(f"logs/{HOST}.txt", "something else\n", "the log changed")
        code, out = self.push(changed)
        self.assertEqual(code, 1)
        self.assertIn(f"{sha[:10]} path: internal or cloud host name", out)
        # --all names the commit that added the path, as a push does (where a rewrite starts), not the newest to change it.
        code, out = self.check("--all")
        self.assertEqual(code, 1)
        self.assertIn(f"{sha[:10]} path: internal or cloud host name", out)
        self.assertNotIn(changed[:10], out)

    def test_a_path_whose_contents_another_holds_is_checked(self):
        # rev-list --objects names each object once, by the first path it meets ("a.txt", "docs"): never by these two.
        self.commit("a.txt", "same\n", "a file")
        file = self.commit(f"logs/{HOST}.txt", "same\n", "the same contents, a private name")
        self.commit("docs/x.txt", "x\n", "a folder")
        folder = self.commit(f"{HOST}/x.txt", "x\n", "the same folder, a private name")
        code, out = self.check("--all")
        self.assertEqual(code, 1)
        self.assertIn(f"{file[:10]} path: internal or cloud host name", out)
        self.assertIn(f"{folder[:10]} path: internal or cloud host name", out)
        self.assertEqual(out.count("internal or cloud host name"), 2)
        code, out = self.push(folder)
        self.assertEqual(code, 1)
        self.assertEqual(out.count("internal or cloud host name"), 2)

    def test_a_branch_name_is_checked(self):
        head = git(self.repo, "rev-parse", "HEAD")
        code, out = self.push(head, ref=f"refs/heads/{DENIED}")
        self.assertEqual(code, 1)
        self.assertIn("pushed ref name: deny-listed string", out)
        self.assertEqual(out.count("deny-listed string"), 1)  # the local name is the same: once
        # Renamed on the way: the remote's name, and the local one (not sent, but one plain push from published).
        code, out = self.push(head, remote_ref=f"refs/heads/{DENIED}")
        self.assertEqual(code, 1)
        self.assertIn("pushed ref name: deny-listed string", out)
        code, out = self.push(head, ref=f"refs/heads/{DENIED}", remote_ref="refs/heads/public")
        self.assertEqual(code, 1)
        self.assertIn("local ref name: deny-listed string", out)
        # Deleting a ref the remote has sends a name already published, and taking it down is the fix: it passes. Deleting one
        # the remote hasn't got (git push origin :refs/heads/x) would send the name for nothing: refused.
        self.assertEqual(self.push(ZERO, ref="(delete)", base=head, remote_ref=f"refs/heads/{DENIED}"), (0, "privacy check: clean\n"))
        code, out = self.push(ZERO, ref="(delete)", remote_ref=f"refs/heads/{DENIED}")
        self.assertEqual(code, 1)
        self.assertIn("deleted ref name: deny-listed string", out)
        # --all: every ref's name; with REVs, the names of the refs they name.
        git(self.repo, "branch", DENIED)
        code, out = self.check("--all")
        self.assertEqual(code, 1)
        self.assertIn("ref name: deny-listed string", out)
        self.assertEqual(self.check("--all", "main"), (0, "privacy check: clean\n"))
        self.assertEqual(self.check("--all", DENIED)[0], 1)
        self.assertEqual(self.check("--all", "--branches")[0], 1)

    def test_a_lightweight_tag_name_is_checked(self):
        head = git(self.repo, "rev-parse", "HEAD")
        git(self.repo, "tag", HOST)
        code, out = self.push(head, ref=f"refs/tags/{HOST}")
        self.assertEqual(code, 1)
        self.assertIn("pushed ref name: internal or cloud host name", out)
        code, out = self.check("--all")
        self.assertEqual(code, 1)
        self.assertIn("ref name: internal or cloud host name", out)
        self.assertEqual(self.check("--all", "--tags")[0], 1)

    def test_a_tagged_tree_is_checked(self):
        # Two trees no commit holds, sharing a file: rev-list --objects names it once, as plain.txt (a-tree's, met first).
        plain = self.tree([("plain.txt", "same\n")])
        private = self.tree([("docs", [(f"{HOST}.txt", "same\n")])])
        git(self.repo, "tag", "a-tree", plain)
        git(self.repo, "tag", "b-tree", private)
        code, out = self.check("--all")
        self.assertEqual(code, 1)
        self.assertIn(f"tree {private[:10]} path: internal or cloud host name", out)
        self.assertEqual(out.count("internal or cloud host name"), 1)
        self.assertEqual(self.check("--all", "a-tree")[0], 0)
        self.assertEqual(self.check("--all", "b-tree")[0], 1)
        git(self.repo, "tag", "-a", "c-tree", "-m", "a tree", private)  # an annotated tag of it, peeled
        self.assertEqual(self.check("--all", "a-tree", "c-tree")[0], 1)

    def test_a_pushed_tag_of_a_tree_or_file_is_checked(self):
        # A tag of a tree publishes its paths and files, a lightweight tag's or an annotated one's; a tag of a file, its contents.
        tree = self.tree([("logs", [(f"{HOST}.txt", f"mail {EMAIL}\n")])])
        for tag in (tree, None):
            if tag is None:
                git(self.repo, "tag", "-a", "t", "-m", "a tree", tree)
                tag = git(self.repo, "rev-parse", "refs/tags/t")
                self.assertNotEqual(tag, tree)
            code, out = self.push(tag, ref="refs/tags/t")
            self.assertEqual(code, 1)
            self.assertIn(f"tree {tree[:10]} path: internal or cloud host name", out)
            self.assertIn(f"tree {tree[:10]} logs/{HOST}.txt:1: email address", out)
        blob = git(self.repo, "hash-object", "-w", "--stdin", stdin=f"built on {HOST}\n")
        code, out = self.push(blob, ref="refs/tags/f")
        self.assertEqual(code, 1)
        self.assertIn(f"blob {blob[:10]}:1: internal or cloud host name", out)
        self.assertEqual(self.push(self.tree([("docs", [("a.txt", "a\n")])]), ref="refs/tags/a"), (0, "privacy check: clean\n"))

    def test_a_display_name_is_checked(self):
        sha = self.commit("a.txt", "a\n", "a change", name=f"Someone {DENIED}")
        code, out = self.push(sha)
        self.assertEqual(code, 1)
        self.assertIn("author name: deny-listed string", out)
        self.assertIn("committer name: deny-listed string", out)

    def test_an_annotated_tag_is_checked(self):
        head = git(self.repo, "rev-parse", "HEAD")
        env = dict(os.environ, GIT_COMMITTER_EMAIL=EMAIL)
        git(self.repo, "tag", "-a", "v1", "-m", f"built on {HOST}", env=env)
        tag = git(self.repo, "rev-parse", "v1")
        self.assertNotEqual(tag, head)
        code, out = self.push(tag, ref="refs/tags/v1")
        self.assertEqual(code, 1)
        self.assertIn("tagger email", out)
        self.assertIn("message", out)
        code, out = self.check("--all")
        self.assertEqual(code, 1)
        self.assertIn("tagger email", out)

    def test_a_git_error_is_exit_2(self):
        self.assertEqual(self.check("--all", "nosuchref")[0], 2)
        self.assertEqual(self.check("--range", "nosuchref..main")[0], 2)

    def test_a_clean_tag_passes(self):
        git(self.repo, "tag", "-a", "v1", "-m", "release one")
        tag = git(self.repo, "rev-parse", "v1")
        self.assertEqual(self.push(tag, ref="refs/tags/v1")[0], 0)


if __name__ == "__main__":
    unittest.main()
