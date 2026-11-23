"""Privacy check for what leaves this box (CLAUDE.md, "Privacy"): email addresses, IP addresses, systems' FQDNs, cloud
resource IDs and keys, certificate thumbprints, and the private strings of a local deny list.

Usage:
    python -I tools/privacy_check.py --pre-push REMOTE URL   the pre-push hook (tools/git-hooks/pre-push): reads git's
                                                             "<local ref> <local sha> <remote ref> <remote sha>" lines on
                                                             stdin and checks the commits the push adds
    python -I tools/privacy_check.py --range A..B            the commits in a range
    python -I tools/privacy_check.py --all [REV ...]         every commit and file in the history (default: all refs)

Each commit's author and committer (email and name), its message, and every file version it adds or changes and its path
are checked, each read whole by its encoding; an annotated tag pushed, its name, tagger and message too; and a push's ref
names, the remote's and the local one (a deletion's only when the remote hasn't the ref). --all checks every commit,
annotated tag, file version and ref name (with REVs, the names of the refs they name) and every path of every commit's
tree. Exit 0: clean; 1: findings (listed); 2: a usage or git error.

The deny list, `.git/info/privacy-deny`, holds private strings (one per line, case-insensitive; `#` starts a comment): the
owner's email and domain, host and account names. It stays in .git/, so it is never committed or pushed: this script, which
is, names none of them. `tools/privacy-allow.txt`, as committed at HEAD, lists literal strings that look like a finding
but aren't (a version number shaped like an IP address).
"""
import re
import subprocess
import sys
from pathlib import Path

ZERO = "0" * 40

# Commit identities that may be published: GitHub's no-reply addresses.
ALLOWED_EMAIL = re.compile(r"(?i)^[A-Za-z0-9._%+-]+@users\.noreply\.github\.com$|^noreply@anthropic\.com$|^noreply@github\.com$")

PATTERNS = [
    ("email address", re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}\b")),
    ("IPv4 address", re.compile(r"(?<![\w.])(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)(?![\w.])")),
    ("internal or cloud host name", re.compile(
        r"(?i)\b[a-z0-9-]+(\.[a-z0-9-]+)*\.(internal|local|lan|corp|intranet|home\.arpa)(?![\w.-])"
        r"|\bec2-\d+-\d+-\d+-\d+\b|\bip-\d+-\d+-\d+-\d+\b|\b[a-z0-9.-]+\.compute\.amazonaws\.com\b|\bEC2AMAZ-[A-Z0-9]+\b")),
    ("AWS resource ID", re.compile(
        r"\b(i|ami|snap|vol|vpc|subnet|sg|eni|igw|rtb|acl|nat|tgw|eipalloc|lt|fs|db)-(?:[0-9a-f]{8}|[0-9a-f]{17})\b")),
    ("AWS region or zone", re.compile(r"\b(ap|us|eu|sa|ca|me|af|il|mx)-(north|south|east|west|central|southeast|northeast|southwest)-\d[a-z]?\b")),
    ("AWS access key", re.compile(r"\b(AKIA|ASIA)[0-9A-Z]{16}\b")),
    ("AWS ARN with an account", re.compile(r"\barn:aws[a-z-]*:[a-z0-9-]*:[a-z0-9-]*:\d{12}:")),
    ("account number", re.compile(r"(?i)\baccount(?: id| number|_id)?\W{0,3}\d{12}\b")),
    ("private key", re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")),
    ("access token", re.compile(r"\b(gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[abpr]-[A-Za-z0-9-]{10,}|sk-[A-Za-z0-9]{32,})\b")),
    ("certificate thumbprint", re.compile(r"(?<![0-9A-Fa-f])[0-9A-F]{40}(?![0-9A-Fa-f])")),
]
# An IPv4-shaped string here is a version, not an address.
VERSION_CONTEXT = re.compile(r"(?i)(version|\bver\b|driverver|\bv|release|build|wdk|sdk|cable|\bvs)\W{0,3}$")


def git(*args, binary=False, stdin=None):
    r = subprocess.run(["git", *args], capture_output=True, input=stdin)
    if r.returncode != 0:
        # Exit 2, as the usage says (SystemExit with a message exits 1, a finding's code).
        print(f"privacy_check: git {' '.join(args)} failed: {r.stderr.decode('utf-8', 'replace').strip()}", file=sys.stderr)
        raise SystemExit(2)
    return r.stdout if binary else r.stdout.decode("utf-8", "replace")


def load_list(path):
    try:
        lines = Path(path).read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return []
    return [l.strip() for l in lines if l.strip() and not l.strip().startswith("#")]


class Checker:
    def __init__(self):
        common = Path(git("rev-parse", "--git-common-dir").strip()).resolve()
        self.deny = [d.lower() for d in load_list(common / "info" / "privacy-deny")]
        # The allow list as committed (a bare repo has no working tree to read it from).
        allow = subprocess.run(["git", "show", "HEAD:tools/privacy-allow.txt"], capture_output=True)
        lines = allow.stdout.decode("utf-8", "replace").splitlines() if allow.returncode == 0 else []
        self.allow = {l.strip() for l in lines if l.strip() and not l.strip().startswith("#")}
        self.findings = []
        self.seen_paths = set()

    def text(self, where, text):
        for n, line in enumerate(text.splitlines(), 1):
            self.line(f"{where}:{n}", line)

    def line(self, where, line):
        low = line.lower()
        for d in self.deny:
            if d in low:
                self.findings.append((where, "deny-listed string", "(from .git/info/privacy-deny)", line))
        for name, rx in PATTERNS:
            for m in rx.finditer(line):
                hit = m.group(0)
                if hit in self.allow:
                    continue
                if name == "email address" and ALLOWED_EMAIL.match(hit):
                    continue
                if name == "IPv4 address" and VERSION_CONTEXT.search(line[: m.start()]):
                    continue
                self.findings.append((where, name, hit, line))

    def person(self, where, header):
        """An author, committer or tagger header: its email must be an allowed identity; its name is checked as text."""
        kind, _, rest = header.partition(" ")
        email = rest[rest.find("<") + 1:rest.find(">")]
        if not ALLOWED_EMAIL.match(email):
            self.findings.append((where, f"{kind} email", email, "set user.email to your GitHub no-reply address"))
        self.line(f"{where} {kind} name", rest[:rest.find("<")].strip())

    def identity(self, commit):
        raw = git("cat-file", "commit", commit)
        for header in raw.split("\n\n", 1)[0].splitlines():
            if header.startswith(("author ", "committer ")):
                self.person(f"commit {commit[:10]}", header)
        self.text(f"commit {commit[:10]} message", raw.split("\n\n", 1)[1] if "\n\n" in raw else "")

    def tag(self, sha):
        """An annotated tag's name, tagger and message; returns the object it points at."""
        head, _, message = git("cat-file", "tag", sha).partition("\n\n")
        target = ""
        for header in head.splitlines():
            if header.startswith("object "):
                target = header[7:].strip()
            elif header.startswith("tag "):
                self.line(f"tag {sha[:10]} name", header[4:])
            elif header.startswith("tagger "):
                self.person(f"tag {sha[:10]}", header)
        self.text(f"tag {sha[:10]} message", message)
        return target

    def path(self, where, path):
        """A file's or folder's path, once each."""
        if path and path not in self.seen_paths:
            self.seen_paths.add(path)
            self.line(f"{where} path", path)

    def commit_diff(self, commit, seen_blobs):
        """A commit's identities and message, and every file version it adds or changes, read whole and by its encoding
        (UTF-16 with a BOM, as the driver's INF is, or UTF-8): a diff's lines would show UTF-16 as bytes."""
        self.identity(commit)
        parents = git("rev-list", "--parents", "-n", "1", commit).split()[1:]
        bases = parents or [None]
        for base in bases:
            args = ["diff-tree", "-r", "-z", "--no-commit-id", "--no-renames"]
            args += [base, commit] if base else ["--root", commit]
            fields = git(*args).split("\0")
            # -z: ":<old mode> <new mode> <old sha> <new sha> <status>" then the path, each NUL-terminated.
            for meta, path in zip(fields[0::2], fields[1::2]):
                parts = meta.split()
                if len(parts) < 5 or parts[4] == "D":
                    continue
                self.path(commit[:10], path)
                if parts[1] == "160000":
                    continue
                sha = parts[3]
                if sha not in seen_blobs:
                    seen_blobs.add(sha)
                    self.blob(sha, f"{commit[:10]} {path}")

    def blob(self, sha, path):
        data = git("cat-file", "-p", sha, binary=True)
        if data[:2] == b"\xff\xfe":
            text = data[2:].decode("utf-16-le", "replace")
        elif b"\0" in data[:8000]:
            return
        else:
            text = data.decode("utf-8", "replace")
        self.text(path, text)

    def report(self):
        if not self.findings:
            print("privacy check: clean")
            return 0
        print(f"privacy check: {len(self.findings)} finding(s); nothing may leave the box with these (CLAUDE.md, \"Privacy\"):")
        for where, name, hit, line in self.findings[:200]:
            print(f"  {where}: {name}: {hit!r}\n      {line.strip()[:160]}")
        if len(self.findings) > 200:
            print(f"  ... and {len(self.findings) - 200} more")
        print("Fix the commits (amend, or rebase and edit), or list a false positive in tools/privacy-allow.txt.")
        return 1


def commits_in(rev_range):
    return git("rev-list", "--reverse", rev_range).split()


def history_paths(revs):
    """(commit, path) for each file a commit of REVs adds or changes against its first parent (a root commit: all its files),
    newest first: so every path of every commit's tree, a merge's too (a path a commit shares with its first parent is the
    parent's, back to the commit that brought it)."""
    fields = git("log", "--format=%H", "--raw", "-z", "--no-renames", "--no-relative", "--root", "--diff-merges=first-parent",
                 "--no-show-signature", "--no-color", *revs).split("\0")
    # -z: each commit's id, then per file ":<old mode> <new mode> <old sha> <new sha> <status>" and its path, each NUL-terminated
    # (a commit's first after a newline). A file's metadata starts with ":", an id never.
    commit, i = "", 0
    while i < len(fields):
        field = fields[i].lstrip("\n")
        if not field.startswith(":"):
            commit, i = field, i + 1
            continue
        if field.split()[-1] != "D":  # a deleted file is in the parent's tree, not this one's
            yield commit, fields[i + 1]
        i += 2


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    c = Checker()
    mode = argv[1]
    seen_blobs = set()
    if mode == "--pre-push":
        remote = argv[2] if len(argv) > 2 else ""
        for line in sys.stdin.read().splitlines():
            parts = line.split()
            if len(parts) != 4:
                continue
            local_ref, local_sha, remote_ref, remote_sha = parts
            if local_sha == ZERO:
                # A deletion pushes no objects, and the name it sends is one the remote already shows: refusing it would only
                # keep a name that leaked from being taken down. Deleting a ref the remote hasn't got (git push origin
                # :refs/heads/x; a short name is refused before the hook) sends a name it has never seen, for nothing: checked.
                if remote_sha == ZERO:
                    c.line("deleted ref name", remote_ref)
                continue
            # The ref names, a branch's or a lightweight tag's as much as an annotated tag's: the remote's is published. The
            # local one isn't sent (git push origin a:b publishes b), but a private name on a branch that's pushed is one plain
            # push from published; in a plain push the two are the same, checked once.
            c.line("pushed ref name", remote_ref)
            if local_ref != remote_ref:
                c.line("local ref name", local_ref)
            # An annotated tag (or a tag of one): its own text, then what it points at.
            while git("cat-file", "-t", local_sha).strip() == "tag":
                local_sha = c.tag(local_sha)
            if git("cat-file", "-t", local_sha).strip() != "commit":
                continue  # a tag of a tree or blob: rare, and no history to walk
            if remote_sha == ZERO:
                # A new branch: what the destination doesn't have yet, by its own remote-tracking branches (none when pushing
                # to a URL, or to a remote never fetched: then every commit is checked). Another remote's commits count as
                # new here: a private remote's history isn't checked for this one.
                new = git("rev-list", "--reverse", local_sha, "--not", f"--remotes={remote}/*").split()
            else:
                new = commits_in(f"{remote_sha}..{local_sha}")
            for commit in new:
                c.commit_diff(commit, seen_blobs)
    elif mode == "--range" and len(argv) == 3:
        for commit in commits_in(argv[2]):
            c.commit_diff(commit, seen_blobs)
    elif mode == "--all":
        revs = argv[2:] or ["--all"]
        # Every ref's name; with REVs, the names of the refs they name (main, origin/main, --tags; not a negative's, ^main).
        if argv[2:]:
            names = [n for n in git("rev-parse", "--revs-only", "--symbolic-full-name", *revs).splitlines() if n.startswith("refs/")]
        else:
            names = git("for-each-ref", "--format=%(refname)").splitlines()
        for name in names:
            c.line("ref name", name)
        # Every path, before the objects: rev-list --objects names each object once, by the first path it meets, so a file
        # whose contents another path also holds (or a folder whose tree another also is) never appears there by its own path.
        for commit, path in history_paths(revs):
            c.path(commit[:10], path)
        # Then each object once (rev-list --objects lists none twice): a commit's identities and message, an annotated tag's
        # text, a file's contents. A tree's or file's path is checked already, unless it's in a tree a ref names directly
        # (a tag of a tree), in no commit's; a tag's "path" is its ref's name, checked above.
        for line in git("rev-list", "--objects", *revs).splitlines():
            sha, _, path = line.partition(" ")
            kind = git("cat-file", "-t", sha).strip()
            if kind in ("tree", "blob"):
                c.path("history", path)
            if kind == "commit":
                c.identity(sha)
            elif kind == "tag":
                c.tag(sha)
            elif kind == "blob":
                c.blob(sha, path)
    else:
        print(__doc__)
        return 2
    return c.report()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
