#!/usr/bin/env python3
"""Generate/check frontend local error codes without changing the wire format."""
from pathlib import Path
import re
import sys


def generated(root: Path) -> str:
    source = (root / "src-tauri/src/errors.rs").read_text(encoding="utf-8")
    entries = re.findall(r'^\s+(\w+) => (\w+) = "([A-Z][A-Z0-9_]*)",$', source, re.MULTILINE)
    if not entries or len({wire for _, _, wire in entries}) != len(entries):
        raise ValueError("Local error code definitions are empty or duplicated")
    rows = "\n".join(f'  {variant}: "{wire}",' for variant, _, wire in entries)
    return ('// Generated from src-tauri/src/errors.rs; run scripts/check-error-codes.py --write.\n'
            'export const ErrorCodes = {\n' + rows + '\n} as const;\n'
            'export type LocalErrorCode = (typeof ErrorCodes)[keyof typeof ErrorCodes];\n'
            '// Older/native errors and upstream codes are deliberately not rejected.\n'
            'export type CompatibleErrorCode = LocalErrorCode | (string & {});\n')


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    path = root / "src/error-codes.ts"
    expected = generated(root)
    if sys.argv[1:] == ["--write"]:
        path.write_text(expected, encoding="utf-8")
        return 0
    if sys.argv[1:]:
        raise ValueError("Only --write is supported")
    errors = []
    if not path.is_file() or path.read_text(encoding="utf-8") != expected:
        errors.append("src/error-codes.ts is out of sync with Rust local codes")
    for source in (root / "src-tauri/src").rglob("*.rs"):
        if source.name.endswith("_tests.rs"):
            continue
        text = source.read_text(encoding="utf-8")
        for match in re.finditer(r'AppError::new\(\s*"[A-Z][A-Z0-9_]*"', text):
            line = text.count("\n", 0, match.start()) + 1
            errors.append(f"{source.relative_to(root)}:{line}: use a canonical local error code")
    for error in errors:
        print(error, file=sys.stderr)
    if not errors:
        print("Local error codes match Rust and TypeScript.")
    return bool(errors)


if __name__ == "__main__":
    sys.exit(main())
