"""Privacy check for what leaves this box (CLAUDE.md, "Privacy"): email addresses, IP addresses, systems' FQDNs, cloud
resource IDs and keys, certificate thumbprints, and the private strings of a local deny list.

Usage:
    python -I tools/privacy_check.py --pre-push REMOTE URL   the pre-push hook (tools/git-hooks/pre-push): reads git's
                                                             "<local ref> <local sha> <remote ref> <remote sha>" lines on
                                                             stdin and checks the commits the push adds
    python -I tools/privacy_check.py --range A..B            the commits in a range
    python -I tools/privacy_check.py --all [REV ...]         every commit and file in the history (default: all refs)

Each commit's author and committer email, its message and the lines its diff adds are checked; --all checks every file
version in full. Exit 0: clean; 1: findings (listed); 2: a usage or git error.

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
        raise SystemExit(f"privacy_check: git {' '.join(args)} failed: {r.stderr.decode('utf-8', 'replace').strip()}")
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

    def identity(self, commit):
        raw = git("cat-file", "commit", commit)
        for header in raw.split("\n\n", 1)[0].splitlines():
            if header.startswith(("author ", "committer ")):
                email = header[header.find("<") + 1:header.find(">")]
                if not ALLOWED_EMAIL.match(email):
                    self.findings.append((f"commit {commit[:10]}", f"{header.split(' ', 1)[0]} email", email,
                                          "set user.email to your GitHub no-reply address"))
        self.text(f"commit {commit[:10]} message", raw.split("\n\n", 1)[1] if "\n\n" in raw else "")

    def commit_diff(self, commit):
        self.identity(commit)
        out = git("show", "--format=", "--unified=0", "--no-color", "--no-ext-diff", "--text", commit)
        path = "?"
        for l in out.splitlines():
            if l.startswith("+++ "):
                path = l[6:] if l.startswith("+++ b/") else l[4:]
            elif l.startswith("+") and not l.startswith("+++"):
                self.line(f"{commit[:10]} {path}", l[1:])

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


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    c = Checker()
    mode = argv[1]
    if mode == "--pre-push":
        for line in sys.stdin.read().splitlines():
            parts = line.split()
            if len(parts) != 4:
                continue
            local_ref, local_sha, remote_ref, remote_sha = parts
            if local_sha == ZERO:
                continue  # a deletion pushes nothing
            if remote_sha == ZERO:
                # A new branch: what no remote branch has yet.
                new = git("rev-list", "--reverse", local_sha, "--not", "--remotes").split()
            else:
                new = commits_in(f"{remote_sha}..{local_sha}")
            for commit in new:
                c.commit_diff(commit)
    elif mode == "--range" and len(argv) == 3:
        for commit in commits_in(argv[2]):
            c.commit_diff(commit)
    elif mode == "--all":
        revs = argv[2:] or ["--all"]
        seen = set()
        for line in git("rev-list", "--objects", *revs).splitlines():
            sha, _, path = line.partition(" ")
            if sha in seen:
                continue
            seen.add(sha)
            kind = git("cat-file", "-t", sha).strip()
            if kind == "commit":
                c.identity(sha)
            elif kind == "blob":
                c.blob(sha, path)
    else:
        print(__doc__)
        return 2
    return c.report()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
