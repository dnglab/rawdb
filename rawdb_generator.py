#!/usr/bin/env python3
"""
rawdb_generator.py — offline rawdb-meta.toml generator.

Walks a `<RAWDB_DIR>/<MAKER>/<MODEL>/...` tree and writes one
`rawdb-meta.toml` per set. Doesn't talk to S3 — the operator pushes the
generated tree with `rclone` / `aws s3 sync` afterwards.

Re-runnable: files seen on disk get a fresh sha256 + tags; files
previously in the meta but no longer on disk are preserved verbatim and
reported as abandoned.

Python 3.11+ (stdlib only).
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import sys
import tomllib
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

META_FILE = "rawdb-meta.toml"
LICENSE_FILE = "LICENSE"
NOTE_FILE = "NOTES"
DEFAULT_LICENSE = "CC0 1.0"
CHUNK = 8 * 1024 * 1024  # 8 MiB — same sweet spot the browser uploader uses

# Files we never include in the [[files]] list.
SKIP_NAMES = {META_FILE, LICENSE_FILE, NOTE_FILE}


# ---- tag extraction -------------------------------------------------------

# Patterns are tried in this exact order: `nocrop` before `crop`, `nodual`
# before `dual`. `_consume` matches without producing a tag so the more
# general pattern further down doesn't fire on the same substring.
_TAG_PATTERNS: list[tuple[re.Pattern[str], object]] = [
    (re.compile(r"\bnocrop\b", re.IGNORECASE), "fullres"),
    (re.compile(r"\bcrop\b", re.IGNORECASE), "crop1.6"),
    (re.compile(r"\bnodual\b", re.IGNORECASE), None),  # consume only
    (re.compile(r"\bdual\b", re.IGNORECASE), "dualpixel"),
    (re.compile(r"\bcraw\b", re.IGNORECASE), "craw"),
    (re.compile(r"\buncompressed\b", re.IGNORECASE), "uncompressed"),
    (re.compile(r"\blossless\b", re.IGNORECASE), "lossless"),
    (re.compile(r"\blossy\b", re.IGNORECASE), "lossy"),
]

_BITS_RE = re.compile(r"\b(\d{1,2})\s*bits?\b", re.IGNORECASE)
_ASPECT_RE = re.compile(r"\b(\d+)\s*[xX]\s*(\d+)\b")


# Normalize a filename stem to a string where `\b` actually fires between
# the human-meaningful tokens. Underscores, dots and hyphens are common
# separators in RAW filenames but `_` is a regex word-char, so
# `\blossless\b` never matches `IMG_lossless_14bit`. Collapse all those
# separators to spaces first.
_SEP_RE = re.compile(r"[_.\-\s]+")


def _normalize_stem(stem: str) -> str:
    return _SEP_RE.sub(" ", stem)


def extract_tags(stem: str) -> list[str]:
    """Lowercase, deduped, sorted tags derived from a filename stem."""
    s = _normalize_stem(stem)
    tags: list[str] = []

    def add(tag: str) -> None:
        if tag and tag.lower() not in {t.lower() for t in tags}:
            tags.append(tag)

    # Order-sensitive token patterns (nocrop before crop, nodual before dual).
    for pat, tag in _TAG_PATTERNS:
        m = pat.search(s)
        if m:
            if tag is not None:
                add(tag)  # type: ignore[arg-type]
            # Remove the matched span so a later pattern can't double-match.
            s = s[: m.start()] + " " + s[m.end():]

    for m in _BITS_RE.finditer(s):
        add(f"{int(m.group(1))}bits")

    for m in _ASPECT_RE.finditer(s):
        w, h = int(m.group(1)), int(m.group(2))
        if w > 0 and h > 0:
            add(f"{w}x{h}")

    return sorted(tags, key=str.lower)


# ---- sha256 ---------------------------------------------------------------


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        while True:
            buf = fh.read(CHUNK)
            if not buf:
                break
            h.update(buf)
    return h.hexdigest()


# ---- TOML writer (matches backend/src/meta.rs::to_toml shape) -------------


def _q(s: str) -> str:
    """TOML basic-string with `\\`, `"`, `\\n`, `\\t` escaped."""
    e = (
        s.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\t", "\\t")
    )
    return f'"{e}"'


def _arr(items: list[str]) -> str:
    return "[" + ", ".join(_q(i) for i in items) + "]"


def render_meta(set_info: "SetInfo", files: list["FileEntry"]) -> str:
    out: list[str] = ["[set]"]
    out.append(f"maker = {_q(set_info.maker)}")
    out.append(f"model = {_q(set_info.model)}")
    out.append(f"license = {_q(set_info.license)}")
    if set_info.uploaded_by:
        out.append(f"uploaded_by = {_q(set_info.uploaded_by)}")
    if set_info.uploaded_at:
        out.append(f"uploaded_at = {_q(set_info.uploaded_at)}")
    if set_info.notes:
        out.append(f"notes = {_q(set_info.notes)}")
    if set_info.special:
        out.append("special = true")

    for f in sorted(files, key=lambda f: f.path):
        out.append("")
        out.append("[[files]]")
        out.append(f"path = {_q(f.path)}")
        if f.sha256:
            out.append(f"sha256 = {_q(f.sha256)}")
        if f.license:
            out.append(f"license = {_q(f.license)}")
        if f.tags:
            out.append(f"tags = {_arr(f.tags)}")
        if f.notes:
            out.append(f"notes = {_q(f.notes)}")
    return "\n".join(out) + "\n"


# ---- data shapes ----------------------------------------------------------


@dataclass
class SetInfo:
    maker: str
    model: str
    license: str
    uploaded_by: str | None = None
    uploaded_at: str | None = None
    notes: str | None = None
    special: bool = False


@dataclass
class FileEntry:
    path: str           # POSIX relative to the set root
    sha256: str | None
    license: str | None
    tags: list[str]
    notes: str | None


@dataclass
class SetResult:
    set_path: Path           # absolute on-disk path to the set
    rel_set: str             # e.g. "Canon/EOS R5"
    new: int = 0
    # `verified`: existing entry's stored sha256 matched the disk file.
    verified: int = 0
    # `unverifiable`: existing entry without a stored sha256 (nothing to
    # compare to), or the file could not be read.
    unverifiable: int = 0
    # (rel_path, claimed_sha or None, computed_sha) for each existing
    # entry whose stored hash disagrees with what's currently on disk.
    mismatches: list[tuple[str, str | None, str]] = field(default_factory=list)
    abandoned_paths: list[str] = field(default_factory=list)
    skipped_paths: list[str] = field(default_factory=list)
    # Stderr lines collected during processing. Workers append here
    # instead of printing directly so each set's diagnostics stay
    # contiguous when emitted by the main thread.
    warnings: list[str] = field(default_factory=list)


# ---- core ------------------------------------------------------------------


def _now_rfc3339() -> str:
    """Current UTC timestamp as RFC 3339 (e.g. `2026-05-23T14:32:11Z`).
    Used to stamp `set.uploaded_at` on a set the script sees for the
    first time."""
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _entry_from_prev(rel: str, prev: dict) -> "FileEntry":
    """Build a FileEntry from a previously-stored meta row, preserving
    all known fields verbatim. Used for both the verify path (existing
    entry still on disk — never rewritten) and the abandoned path
    (entry preserved even though the file is gone)."""
    return FileEntry(
        path=rel,
        sha256=prev.get("sha256") if isinstance(prev.get("sha256"), str) else None,
        license=prev.get("license") if isinstance(prev.get("license"), str) else None,
        tags=list(prev.get("tags") or []),
        notes=prev.get("notes") if isinstance(prev.get("notes"), str) else None,
    )


def read_first_license_line(set_path: Path) -> str:
    f = set_path / LICENSE_FILE
    if not f.is_file():
        return DEFAULT_LICENSE
    try:
        with f.open("r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                stripped = line.strip()
                if stripped:
                    return stripped
    except OSError as e:
        print(f"warning: could not read {f}: {e}", file=sys.stderr)
    return DEFAULT_LICENSE


def read_note_file(set_path: Path) -> str | None:
    """Whole NOTES file content (trailing whitespace stripped); `None`
    if absent or empty. Used as the initial `set.notes` value when a set
    is being scraped for the first time."""
    f = set_path / NOTE_FILE
    if not f.is_file():
        return None
    try:
        text = f.read_text(encoding="utf-8", errors="replace").rstrip()
    except OSError as e:
        print(f"warning: could not read {f}: {e}", file=sys.stderr)
        return None
    return text or None


def load_existing_meta(set_path: Path) -> tuple[dict | None, dict[str, dict]]:
    """Parse the existing meta if any; return (raw_doc, {path: file_entry})."""
    f = set_path / META_FILE
    if not f.is_file():
        return None, {}
    try:
        with f.open("rb") as fh:
            doc = tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError) as e:
        print(
            f"warning: could not parse existing {f.relative_to(f.parents[2])} "
            f"({e}); starting from empty",
            file=sys.stderr,
        )
        return None, {}
    by_path: dict[str, dict] = {}
    for entry in doc.get("files", []) or []:
        if isinstance(entry, dict) and isinstance(entry.get("path"), str):
            by_path[entry["path"]] = entry
    return doc, by_path


def walk_set_files(set_path: Path) -> tuple[list[Path], list[str]]:
    """Return (file_paths, skipped_relpaths). file_paths have at least one
    category subdir between set root and the file."""
    files: list[Path] = []
    skipped: list[str] = []
    set_root = set_path.resolve()
    for path in sorted(set_path.rglob("*")):
        if not path.is_file():
            continue
        if path.name.startswith("."):
            continue
        if path.name in SKIP_NAMES:
            continue
        # Reject symlinks pointing outside the set.
        try:
            resolved = path.resolve()
        except OSError:
            continue
        try:
            resolved.relative_to(set_root)
        except ValueError:
            skipped.append(str(path.relative_to(set_path)))
            continue
        rel = path.relative_to(set_path)
        if len(rel.parts) < 2:
            # No category folder — the schema requires one.
            skipped.append(rel.as_posix())
            continue
        files.append(path)
    return files, skipped


def process_set(set_path: Path, rel_set: str) -> SetResult:
    maker, model = rel_set.split("/", 1)
    license_str = read_first_license_line(set_path)
    _existing_doc, existing_by_path = load_existing_meta(set_path)

    set_info = SetInfo(maker=maker, model=model, license=license_str)
    # Preserve curated set-level fields if present.
    if _existing_doc:
        s = _existing_doc.get("set") or {}
        if isinstance(s, dict):
            set_info.uploaded_by = (s.get("uploaded_by") or None) or None
            ua = s.get("uploaded_at")
            if isinstance(ua, str):
                set_info.uploaded_at = ua
            else:
                # tomllib parses datetimes — re-emit as RFC3339-ish.
                set_info.uploaded_at = ua.isoformat() if ua else None
            set_info.notes = (s.get("notes") or None) or None
            set_info.special = bool(s.get("special") or False)
    else:
        # First-time scrape: seed set.notes from a NOTE file in the set
        # folder if present. Once the meta exists its `notes` value wins
        # so later UI edits aren't overwritten on re-runs.
        set_info.notes = read_note_file(set_path)
        # Stamp the moment we first saw this set. Preserved on every
        # subsequent run (the `if _existing_doc:` branch above carries
        # the value forward), so this acts as the set's discovery time.
        set_info.uploaded_at = _now_rfc3339()

    disk_files, skipped = walk_set_files(set_path)
    seen_paths: set[str] = set()
    entries: list[FileEntry] = []
    result = SetResult(set_path=set_path, rel_set=rel_set, skipped_paths=skipped)

    for fpath in disk_files:
        rel = fpath.relative_to(set_path).as_posix()
        seen_paths.add(rel)

        if rel in existing_by_path:
            # Existing entry: preserve verbatim. Only re-hash to verify
            # the stored claim — never overwrite tags / sha256 / notes /
            # license on a row that's already curated.
            prev = existing_by_path[rel]
            entries.append(_entry_from_prev(rel, prev))
            claimed = prev.get("sha256") if isinstance(prev.get("sha256"), str) else None
            if claimed is None:
                result.unverifiable += 1
                continue
            try:
                computed = sha256_file(fpath)
            except OSError as e:
                result.warnings.append(
                    f"warning: could not verify {rel_set}/{rel}: {e}"
                )
                result.unverifiable += 1
                continue
            if computed.lower() == claimed.lower():
                result.verified += 1
            else:
                result.mismatches.append((rel, claimed, computed))
            continue

        # New file (not in the existing meta): fresh hash + tag extraction.
        stem = Path(fpath.name).stem
        try:
            digest = sha256_file(fpath)
        except OSError as e:
            result.warnings.append(
                f"warning: failed to hash {rel_set}/{rel}: {e}"
            )
            continue
        entries.append(
            FileEntry(
                path=rel,
                sha256=digest,
                license=None,
                tags=extract_tags(stem),
                notes=None,
            )
        )
        result.new += 1

    # Abandoned: previously declared but no longer on disk. Preserve verbatim.
    for rel, prev in existing_by_path.items():
        if rel in seen_paths:
            continue
        result.abandoned_paths.append(rel)
        entries.append(_entry_from_prev(rel, prev))

    # Only rewrite the meta when something structurally changed: a new
    # file appeared on disk, or an entry was abandoned. If everything
    # we found is already in the meta (regardless of mismatch outcomes),
    # leave the file untouched — that preserves operator-curated
    # field/file ordering, hand-written comments, etc., and matches the
    # "do not modify existing entries" mandate at the file level.
    existed_before = _existing_doc is not None
    structural_change = result.new > 0 or bool(result.abandoned_paths)
    if not existed_before or structural_change:
        meta_text = render_meta(set_info, entries)
        out_path = set_path / META_FILE
        out_path.write_text(meta_text, encoding="utf-8")

    # Per-set warnings — collected, not printed. main() flushes them in
    # contiguous per-set blocks as each future completes.
    for rel in result.abandoned_paths:
        result.warnings.append(
            f"warning: abandoned (no longer on disk): {rel_set}/{rel}"
        )
    for rel in result.skipped_paths:
        result.warnings.append(
            f"warning: skipped (no category folder): {rel_set}/{rel}"
        )
    return result


# ---- set discovery --------------------------------------------------------


def discover_sets(rawdb_dir: Path, sub_dir: str | None) -> list[tuple[Path, str]]:
    """Yield (absolute_set_path, "Maker/Model") pairs to process."""
    rawdb_dir = rawdb_dir.resolve()
    if not rawdb_dir.is_dir():
        print(f"error: RAWDB_DIR not a directory: {rawdb_dir}", file=sys.stderr)
        sys.exit(2)

    if sub_dir:
        candidate = (rawdb_dir / sub_dir).resolve()
        try:
            candidate.relative_to(rawdb_dir)
        except ValueError:
            print(
                f"error: SUB_DIR resolves outside RAWDB_DIR: {sub_dir}",
                file=sys.stderr,
            )
            sys.exit(2)
        if not candidate.is_dir():
            print(f"error: SUB_DIR not a directory: {candidate}", file=sys.stderr)
            sys.exit(2)
        parts = candidate.relative_to(rawdb_dir).parts
        if len(parts) == 1:
            # maker-level
            maker_dir = candidate
            return _iter_models(rawdb_dir, maker_dir)
        if len(parts) == 2:
            # specific set
            return [(candidate, candidate.relative_to(rawdb_dir).as_posix())]
        print(
            f"error: SUB_DIR must be a maker or maker/model: {sub_dir}",
            file=sys.stderr,
        )
        sys.exit(2)

    # No SUB_DIR — walk every maker.
    out: list[tuple[Path, str]] = []
    for maker_dir in sorted(p for p in rawdb_dir.iterdir() if p.is_dir()):
        out.extend(_iter_models(rawdb_dir, maker_dir))
    return out


def _iter_models(
    rawdb_dir: Path, maker_dir: Path
) -> list[tuple[Path, str]]:
    out: list[tuple[Path, str]] = []
    for model_dir in sorted(p for p in maker_dir.iterdir() if p.is_dir()):
        rel = model_dir.relative_to(rawdb_dir).as_posix()
        out.append((model_dir, rel))
    return out


# ---- main -----------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Generate rawdb-meta.toml files for a local samples tree.",
    )
    ap.add_argument("rawdb_dir", help="Root directory containing <MAKER>/<MODEL>/ sets")
    ap.add_argument(
        "sub_dir",
        nargs="?",
        default=None,
        help="Optional path relative to RAWDB_DIR (a maker or a single set)",
    )
    ap.add_argument(
        "-j",
        "--jobs",
        type=int,
        default=os.cpu_count() or 1,
        help="Parallel set workers (default: number of CPU cores). "
             "Pass 1 to force serial execution.",
    )
    args = ap.parse_args(argv)

    if args.jobs < 1:
        print(f"error: --jobs must be >= 1 (got {args.jobs})", file=sys.stderr)
        return 2

    rawdb_dir = Path(args.rawdb_dir)
    sets = discover_sets(rawdb_dir, args.sub_dir)
    if not sets:
        print("note: no sets found; nothing to do.")
        return 0

    # Cap workers at the actual workload so we don't spin up threads
    # that have nothing to do (one worker per set is the upper bound).
    workers = min(args.jobs, len(sets))
    print(f"using {workers} worker(s) across {len(sets)} set(s)")

    total_files = 0
    total_new = 0
    total_verified = 0
    total_unverifiable = 0
    total_abandoned = 0
    mismatch_list: list[tuple[str, str | None, str]] = []
    abandoned_list: list[str] = []
    skipped_list: list[str] = []

    # Sets are independent (each writes its own meta in its own folder)
    # so we parallelize across them. hashlib.sha256 + file I/O both
    # release the GIL, so threads scale here without process overhead.
    with ThreadPoolExecutor(max_workers=workers) as ex:
        fut_to_set = {
            ex.submit(process_set, sp, rs): rs for sp, rs in sets
        }
        for fut in as_completed(fut_to_set):
            rel_set = fut_to_set[fut]
            try:
                r = fut.result()
            except Exception as e:
                # Surface the failure but keep draining the pool — one
                # broken set shouldn't kill the run.
                print(
                    f"error: processing {rel_set} failed: {e}",
                    file=sys.stderr,
                )
                continue
            # Flush this set's warnings as one contiguous block.
            for w in r.warnings:
                print(w, file=sys.stderr)
            total_files += (
                r.new + r.verified + r.unverifiable
                + len(r.mismatches) + len(r.abandoned_paths)
            )
            total_new += r.new
            total_verified += r.verified
            total_unverifiable += r.unverifiable
            total_abandoned += len(r.abandoned_paths)
            mismatch_list.extend(
                (f"{rel_set}/{p}", claimed, computed)
                for p, claimed, computed in r.mismatches
            )
            abandoned_list.extend(f"{rel_set}/{p}" for p in r.abandoned_paths)
            skipped_list.extend(f"{rel_set}/{p}" for p in r.skipped_paths)
    # Stable summary tail — sort the lists so output is deterministic
    # regardless of completion order.
    mismatch_list.sort(key=lambda t: t[0])
    abandoned_list.sort()
    skipped_list.sort()

    total_mismatched = len(mismatch_list)
    print(f"\nProcessed {total_files} file(s) across {len(sets)} set(s).")
    print(f"  New:          {total_new}")
    print(f"  Verified:     {total_verified}")
    print(f"  Mismatched:   {total_mismatched}")
    print(f"  Unverifiable: {total_unverifiable}")
    print(f"  Abandoned:    {total_abandoned}")
    if mismatch_list:
        print("Mismatched checksums (existing meta vs. disk):")
        for path, claimed, computed in mismatch_list:
            print(f"  {path}")
            print(f"    claimed:  {claimed if claimed else '(none)'}")
            print(f"    computed: {computed}")
    if abandoned_list:
        print("Abandoned files:")
        for p in abandoned_list:
            print(f"  {p}")
    if skipped_list:
        print("Skipped (no category folder):")
        for p in skipped_list:
            print(f"  {p}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
