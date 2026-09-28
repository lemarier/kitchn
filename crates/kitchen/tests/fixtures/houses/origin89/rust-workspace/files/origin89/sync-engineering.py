#!/usr/bin/env python3
"""Refresh Origin89 skills from the public engineering repo into an ignored cache."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import tarfile
import tempfile
import urllib.error
import urllib.request

SOURCE = "origin89hq/engineering"
HEAD_URL = f"https://api.github.com/repos/{SOURCE}/commits/main"
MAX_DOWNLOAD = 2 * 1024 * 1024
MAX_CONTENT = 4 * 1024 * 1024
SHA = re.compile(r"[0-9a-f]{40}")
NAME = re.compile(r"origin89-[a-z0-9]+(?:-[a-z0-9]+)*")


def download(url):
    request = urllib.request.Request(url, headers={"User-Agent": "origin89-skills-sync/1"})
    with urllib.request.urlopen(request, timeout=10) as response:
        data = response.read(MAX_DOWNLOAD + 1)
    if len(data) > MAX_DOWNLOAD:
        raise ValueError("Engineering download exceeds the 2 MiB limit")
    return data


def skill_files(archive):
    """Read only regular files under namespaced skill folders; never extract tar paths."""
    files = {}
    total = 0
    entries = 0
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as bundle:
        for entry in bundle:
            entries += 1
            total += entry.size
            if entries > 1024 or total > MAX_CONTENT or entry.size < 0:
                raise ValueError("Engineering archive exceeds its size or entry limit")
            parts = PurePosixPath(entry.name).parts
            if len(parts) < 3 or parts[1] != "skills":
                continue
            if entry.name.startswith("/") or "\\" in entry.name or ".." in parts:
                raise ValueError("Unsafe path in skill archive")
            if entry.isdir():
                continue
            if len(parts) < 4 or not NAME.fullmatch(parts[2]) or not entry.isfile():
                raise ValueError("Only regular files in origin89-* skill folders are allowed")
            relative = Path(*parts[1:])
            if relative in files or entry.size > 256 * 1024:
                raise ValueError("Duplicate or oversized skill file")
            if len(files) >= 256:
                raise ValueError("Skill archive exceeds its file or content limit")
            data = bundle.extractfile(entry).read()
            data.decode("utf-8")
            files[relative] = data
    names = {path.parts[1] for path in files}
    if not {"origin89-working", "origin89-writing", "origin89-commits"} <= names:
        raise ValueError("Archive is missing the shared working, writing, or commit skill")
    for name in names:
        entry = files.get(Path("skills") / name / "SKILL.md", b"").decode()
        sections = entry.split("---", 2)
        if len(sections) != 3 or sections[0].strip():
            raise ValueError(f"Missing skill frontmatter: {name}")
        header = sections[1].splitlines()
        if f"name: {name}" not in header or not any(
            line.startswith("description:") and line.partition(":")[2].strip()
            for line in header
        ):
            raise ValueError(f"Invalid skill name or description: {name}")
    return files, sorted(names)


def directory(path):
    """Reject redirected cache/discovery directories before creating children."""
    if path.is_symlink() or (path.exists() and not path.is_dir()):
        raise ValueError(f"Expected a normal directory: {path}")
    path.mkdir(exist_ok=True)


def snapshot_state(snapshot, revision):
    if snapshot.is_symlink() or not snapshot.is_dir():
        raise ValueError("Expected an immutable snapshot directory")
    state_path = snapshot / "state.json"
    if state_path.is_symlink() or state_path.stat().st_size > 64 * 1024:
        raise ValueError("Invalid cached state file")
    state = json.loads(state_path.read_text())
    if not isinstance(state, dict):
        raise ValueError("Invalid cached state")
    if state.get("source") != SOURCE or state.get("revision") != revision:
        raise ValueError("Invalid cached source or revision")
    names = state.get("skills")
    if not isinstance(names, list) or not names or any(not isinstance(n, str) for n in names):
        raise ValueError("Invalid cached skill list")
    if len(names) != len(set(names)):
        raise ValueError("Duplicate cached skill names")
    for name in names:
        if not isinstance(name, str) or not NAME.fullmatch(name):
            raise ValueError("Invalid cached skill name")
        if not (snapshot / "skills" / name / "SKILL.md").is_file():
            raise ValueError(f"Missing cached skill: {name}")
    hashes = state.get("files")
    if not isinstance(hashes, dict) or not hashes or len(hashes) > 256:
        raise ValueError("Missing cached file hashes")
    actual = set()
    total = 0
    for path in snapshot.rglob("*"):
        if path.is_symlink():
            raise ValueError("Symlink inside cached snapshot")
        if path.is_file() and path != state_path:
            relative = path.relative_to(snapshot).as_posix()
            total += path.stat().st_size
            if path.stat().st_size > 256 * 1024 or total > MAX_CONTENT:
                raise ValueError("Oversized cached skill file")
            if hashes.get(relative) != hashlib.sha256(path.read_bytes()).hexdigest():
                raise ValueError(f"Cached skill was modified: {relative}")
            actual.add(relative)
    if actual != set(hashes):
        raise ValueError("Cached files do not match their manifest")
    return state


def current_state(cache):
    pointer = cache / "current"
    if not pointer.is_symlink():
        if pointer.exists():
            raise ValueError("Managed current pointer is not a symlink")
        return None
    target = os.readlink(pointer)
    revision = Path(target).name
    if not SHA.fullmatch(revision) or target != f"versions/{revision}":
        raise ValueError("Unexpected managed cache pointer")
    versions = cache / "versions"
    if versions.is_symlink():
        raise ValueError("Redirected snapshot directory")
    return snapshot_state(versions / revision, revision)


def activate(project, cache, state, previous):
    old_names = set(previous["skills"]) if previous else set()
    new_names = set(state["skills"])
    targets = {}
    # Check every discovery directory before changing links or the shared pointer.
    for assistant in (".agents", ".claude"):
        directory(project / assistant)
        discovery = project / assistant / "skills"
        directory(discovery)
        for name in sorted(old_names | new_names):
            link = discovery / name
            target = os.path.relpath(cache / "current" / "skills" / name, discovery)
            if link.is_symlink():
                if os.readlink(link) != target:
                    raise ValueError(f"Refusing to replace a custom skill link: {link}")
            elif link.exists():
                raise ValueError(f"Refusing to replace local skill files: {link}")
            targets[link] = target
    next_pointer = cache / "next"
    if next_pointer.exists() or next_pointer.is_symlink():
        raise ValueError("Unexpected pending cache pointer; inspect it before retrying")
    created = []
    try:
        for link, target in targets.items():
            if link.name in new_names and not link.is_symlink():
                link.symlink_to(target, target_is_directory=True)
                created.append(link)
        next_pointer.symlink_to(f"versions/{state['revision']}", target_is_directory=True)
        created.append(next_pointer)
        os.replace(next_pointer, cache / "current")
    except OSError:
        # Before the pointer changes, remove only links created by this attempt.
        for link in reversed(created):
            link.unlink()
        raise
    for link in targets:
        if link.name not in new_names and link.is_symlink():
            link.unlink()


def refresh(project, fetch=download, offline=False):
    project = Path(project).resolve(strict=True)
    directory(project / ".origin89")
    cache = project / ".origin89" / "engineering"
    directory(cache)
    lock = cache / "sync.lock"
    try:
        lock.mkdir()
    except FileExistsError as error:
        raise ValueError("Another refresh is active; inspect sync.lock if it was interrupted") from error
    try:
        previous = current_state(cache)
        reason = "offline requested" if offline else None
        state = None
        if not offline:
            try:
                metadata = json.loads(fetch(HEAD_URL))
                revision = metadata.get("sha") if isinstance(metadata, dict) else None
                if not isinstance(revision, str) or not SHA.fullmatch(revision):
                    raise ValueError("GitHub did not return a valid engineering revision")
                if previous and previous["revision"] == revision:
                    state = previous
                else:
                    archive = fetch(f"https://codeload.github.com/{SOURCE}/tar.gz/{revision}")
            except (urllib.error.URLError, TimeoutError, ConnectionError) as error:
                reason = f"refresh unavailable: {error}"
            if reason is None and state is None:
                files, names = skill_files(archive)
                versions = cache / "versions"
                directory(versions)
                target = versions / revision
                state = {"source": SOURCE, "revision": revision, "skills": names,
                         "files": {path.as_posix(): hashlib.sha256(data).hexdigest()
                                   for path, data in files.items()}}
                with tempfile.TemporaryDirectory(prefix="prepare-", dir=cache) as scratch:
                    stage = Path(scratch) / "snapshot"
                    stage.mkdir()
                    for path, data in files.items():
                        dest = stage / path
                        dest.parent.mkdir(parents=True, exist_ok=True)
                        dest.write_bytes(data)
                    (stage / "state.json").write_text(json.dumps(state, indent=2) + "\n")
                    if target.exists() or target.is_symlink():
                        if snapshot_state(target, revision) != state:
                            raise ValueError("Existing snapshot does not match the upstream revision")
                    else:
                        stage.rename(target)
        if reason is not None:
            if previous is None:
                raise ValueError(f"No cached engineering skills available ({reason})")
            state = previous
        activate(project, cache, state, previous)
        return {key: state[key] for key in ("source", "revision", "skills")} | {
            "path": str(cache / "versions" / state["revision"]), "cached": reason}
    finally:
        lock.rmdir()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--offline", action="store_true", help="Use and report the existing cached revision")
    args = parser.parse_args()
    try:
        result = refresh(args.project, offline=args.offline)
    except (ValueError, OSError, tarfile.TarError) as error:
        parser.exit(1, f"Engineering skills: {error}\n")
    print(json.dumps(result, indent=2))
    if result["cached"]:
        print("Using cached skills; this run did not confirm the latest upstream content.")


if __name__ == "__main__":
    main()
