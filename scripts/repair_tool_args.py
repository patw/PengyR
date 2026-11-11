#!/usr/bin/env python3
"""Repair Pengy chat histories that carry malformed tool-call arguments.

Why: a tool call whose `function.arguments` string is truncated or otherwise not
valid JSON is stored verbatim in the chat file. Pengy replays the whole history
on every turn, so a single bad entry poisons the chat forever on any backend
that validates the arguments (Basalt/Strata answer 400 "Expecting ',' delimiter"
at the position where the stored string ran out). Lenient hosted APIs ignore it,
which is why the same chat still works on `fast`/`smart`.

The repair never changes assistant/tool pairing: the call's id/type/name and its
tool result stay exactly as they are; only the unparseable arguments string is
replaced by a small, honest handle recorded as JSON.

Usage:
  repair_tool_args.py [--apply] [--json] [--no-backup] [FILE ...]

  no FILE   -> every ~/.config/pengy/chats/*.json
  default   -> dry run: report only, write nothing
  --apply   -> back up each file (already-applied files are skipped) and rewrite

Exit codes: 0 clean/ok, 1 nothing to do AND --apply given? no: 0 always unless a
file could not be read or an applied file still fails verification (2).
"""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import sys
import time
from pathlib import Path

HANDLE_NOTE = (
    "The original arguments for this call were not valid JSON (a truncated or "
    "malformed generation) and the call was never run. Detail kept: character "
    "count, sha256 and a short preview."
)


def chat_paths(argv: list[str]) -> list[Path]:
    plain = [a for a in argv if not a.startswith("-")]
    if plain:
        return [Path(p).expanduser() for p in plain]
    return sorted(Path(os.path.expanduser("~/.config/pengy/chats")).glob("*.json"))


def handle_for(raw: str) -> str:
    head = raw[:160]
    return json.dumps(
        {
            "_repaired": True,
            "note": HANDLE_NOTE,
            "chars": len(raw),
            "sha256": hashlib.sha256(raw.encode("utf-8")).hexdigest()[:16],
            "preview": head,
        },
        ensure_ascii=False,
    )


def scan(doc) -> list[dict]:
    """Every unparseable arguments string in a chat document, with its address."""
    found = []
    for mi, msg in enumerate(doc.get("messages") or []):
        if not isinstance(msg, dict):
            continue
        for ci, call in enumerate(msg.get("tool_calls") or []):
            fn = (call or {}).get("function") or {}
            args = fn.get("arguments")
            if not isinstance(args, str) or not args.strip():
                continue
            try:
                json.loads(args)
            except Exception as exc:  # noqa: BLE001 - any parse failure counts
                found.append(
                    {
                        "message": mi,
                        "call": ci,
                        "id": call.get("id"),
                        "tool": fn.get("name"),
                        "chars": len(args),
                        "error": str(exc),
                    }
                )
    return found


def repair(doc, hits: list[dict]) -> None:
    for h in hits:
        call = doc["messages"][h["message"]]["tool_calls"][h["call"]]
        call["function"]["arguments"] = handle_for(call["function"]["arguments"])


def main(argv: list[str]) -> int:
    apply = "--apply" in argv
    as_json = "--json" in argv
    backup = "--no-backup" not in argv
    paths = chat_paths(argv)
    report = {"apply": apply, "files": [], "files_changed": 0, "calls_repaired": 0, "errors": []}

    for path in paths:
        entry = {"file": str(path), "title": None, "calls": []}
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
        except Exception as exc:  # noqa: BLE001
            report["errors"].append({"file": str(path), "error": str(exc)})
            report["files"].append(entry)
            continue
        entry["title"] = doc.get("title")
        hits = scan(doc)
        entry["calls"] = hits
        if hits and apply:
            if backup:
                stamp = time.strftime("%Y%m%d-%H%M%S")
                shutil.copy2(path, path.with_name(f"{path.name}.bak-repair-{stamp}"))
            repair(doc, hits)
            path.write_text(json.dumps(doc, ensure_ascii=False, indent=2), encoding="utf-8")
            if scan(json.loads(path.read_text(encoding="utf-8"))):
                report["errors"].append({"file": str(path), "error": "still invalid after repair"})
        if hits:
            report["files_changed"] += 1
            report["calls_repaired"] += len(hits)
        report["files"].append(entry)

    if as_json:
        print(json.dumps(report, indent=2, ensure_ascii=False))
    else:
        verb = "repaired" if apply else "would repair"
        for entry in report["files"]:
            if not entry["calls"]:
                continue
            print(f"{entry['file']}\n  {entry['title']!r}")
            for c in entry["calls"]:
                print(
                    f"    msg {c['message']:>4}  {c['tool']:<16} {c['chars']:>6} chars  "
                    f"{c['error']}"
                )
        print(
            f"\n{verb} {report['calls_repaired']} call(s) in "
            f"{report['files_changed']} chat file(s); scanned {len(paths)} file(s)."
        )
        if not apply and report["calls_repaired"]:
            print("Dry run — nothing written. Re-run with --apply.")
        for err in report["errors"]:
            print(f"ERROR {err['file']}: {err['error']}", file=sys.stderr)
    return 2 if report["errors"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
