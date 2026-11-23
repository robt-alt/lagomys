"""Compare the canonical scripts with the approved o200k_base budgets."""

from pathlib import Path
import re

import tiktoken


ROOT = Path(__file__).resolve().parents[1]
BUDGETS = (("deployment diagnosis", 52), ("smoke test", 39))


def main():
    syntax = (ROOT / "docs" / "syntax.md").read_text(encoding="utf-8")
    canonical = syntax.split("## 定稿样本", maxsplit=1)[1]
    scripts = re.findall(r"^```text\n(.*?)\n```$", canonical, re.MULTILINE | re.DOTALL)
    if len(scripts) != len(BUDGETS):
        raise SystemExit("Documented script samples do not match the token gates")

    encoding = tiktoken.get_encoding("o200k_base")
    failed = False
    for script, (name, limit) in zip(scripts, BUDGETS):
        count = len(encoding.encode_ordinary(script))
        print(f"{name}: {count}/{limit} tokens")
        failed |= count > limit
    raise SystemExit(failed)


if __name__ == "__main__":
    main()
