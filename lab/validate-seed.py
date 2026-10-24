#!/usr/bin/env python3
"""Validate a cloud-init user-data file before it is burned into a seed ISO.

A malformed seed is an expensive failure: the guest boots, the console works,
but every directive after the parse error is silently skipped, so the root
password and the network configuration never apply. Checking here turns a
confusing half-configured guest into a clear error at build time.
"""
import sys

try:
    import yaml
except ImportError:
    print("validate-seed: PyYAML is not installed; cannot validate", file=sys.stderr)
    sys.exit(2)


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    path = sys.argv[1]
    try:
        with open(path, encoding="utf-8") as handle:
            doc = yaml.safe_load(handle)
    except Exception as exc:  # noqa: BLE001 - report whatever the parser said
        print(f"validate-seed: {path} is not valid YAML: {exc}", file=sys.stderr)
        return 1

    if not isinstance(doc, dict):
        print("validate-seed: document is not a mapping", file=sys.stderr)
        return 1

    problems = []
    files = doc.get("write_files")
    if not files:
        problems.append("no write_files section")
    else:
        for entry in files:
            if "path" not in entry or "content" not in entry:
                problems.append(f"write_files entry missing path/content: {entry!r}")
    if "chpasswd" not in doc:
        problems.append("no chpasswd section (root would keep its locked password)")

    # FreeBSD's nuageinit is not full cloud-init: it writes each runcmd entry
    # with f:write(c .. "\n"), so an array entry (cloud-init's {command: [...]}
    # form) raises "attempt to concatenate a table value" and aborts the rest of
    # the script. Catching it here is much cheaper than diagnosing a guest whose
    # files were written but whose commands never ran.
    for index, entry in enumerate(doc.get("runcmd") or []):
        if not isinstance(entry, str):
            problems.append(
                f"runcmd[{index}] is {type(entry).__name__}, not a string; "
                "nuageinit cannot write array-form commands"
            )

    if problems:
        for problem in problems:
            print(f"validate-seed: {problem}", file=sys.stderr)
        return 1

    print(
        f"seed validated: {len(files)} file(s), "
        f"chpasswd={'yes' if 'chpasswd' in doc else 'no'}, "
        f"runcmd={len(doc.get('runcmd') or [])} step(s)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
