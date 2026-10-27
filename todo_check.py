"""Check that TODO.md / TODO_ARCHIVE.md entries stay short, and print the next free number.

Run from anywhere:  python todo_check.py
Exits 1 if an entry or the file header is over its limit, so it can gate like a lint.
Long reasoning belongs in doc/todo/NN-slug.md, linked from the entry. Adapted from the TMR
workspace's todo_check.py; TODO_ARCHIVE.md is optional until the first item closes.
"""
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ENTRY_LIMIT = {"TODO.md": 3000, "TODO_ARCHIVE.md": 1200}  # bytes per entry
HEADER_LIMIT = 1000  # bytes before the first entry


def split(text):
    """Return (header_bytes, [(number, title, bytes)]) for every '### N.' entry."""
    lines = text.split("\n")
    heads = [i for i, l in enumerate(lines) if l.startswith("### ") or l.startswith("## ")]
    first = next((i for i in heads if lines[i].startswith("### ")), len(lines))
    header = sum(len(l.encode()) + 1 for l in lines[:first])
    out = []
    for a, b in zip(heads, heads[1:] + [len(lines)]):
        m = re.match(r"### ([0-9/]+[a-z]?)\. (.*)", lines[a])
        if not m:
            continue
        while b > a + 1 and lines[b - 1].strip() in ("", "---"):
            b -= 1
        out.append((m.group(1), m.group(2), sum(len(l.encode()) + 1 for l in lines[a:b])))
    return header, out


def main():
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")  # Windows console is cp1252
    failed = False
    numbers = []
    for name, limit in ENTRY_LIMIT.items():
        path = HERE / name
        if not path.exists():
            continue
        text = path.read_text(encoding="utf-8")
        header, entries = split(text)
        over = [e for e in entries if e[2] > limit]
        print(f"{name}: {len(entries)} entries, {len(text.encode()) / 1024:.1f} KB, "
              f"entry limit {limit:,} B, header {header:,} B")
        if header > HEADER_LIMIT:
            print(f"  OVER  header is {header:,} B (limit {HEADER_LIMIT:,})")
            failed = True
        for num, title, size in sorted(over, key=lambda e: -e[2]):
            print(f"  OVER  #{num:5s} {size:6,} B  {title[:70]}")
            failed = True
        for num, _, _ in entries:
            numbers += num.split("/")
    dupes = sorted({n for n in numbers if numbers.count(n) > 1}, key=lambda n: int(re.match(r"\d+", n)[0]))
    numbers = [int(re.match(r"\d+", n)[0]) for n in numbers]
    if dupes:
        print(f"  DUPLICATE numbers across both files: {dupes}")
        failed = True
    print(f"Next free number: {max(numbers, default=0) + 1}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
