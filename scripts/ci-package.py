#!/usr/bin/env python3
"""Smoke-test and package a native release binary using only Python's stdlib."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parent.parent


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def selected_license(expression):
    """Choose a permissive branch in the SPDX forms used by Cargo.lock."""
    normalized = expression.replace("/", " OR ")
    if normalized.startswith("(MIT OR Apache-2.0) AND "):
        selected = normalized.replace("(MIT OR Apache-2.0)", "MIT", 1)
    elif " AND " not in normalized and " OR " in normalized:
        options = [option.strip() for option in normalized.split(" OR ")]
        selected = next(
            (preferred for preferred in ("MIT", "Apache-2.0", "BSD-3-Clause") if preferred in options),
            options[0],
        )
    else:
        selected = normalized
    if re.search(r"\b(?:A|L)?GPL-", selected):
        raise ValueError(f"unapproved copyleft license selection: {expression}")
    return selected


def notice_files(crate, name):
    """Find notice texts without walking source, build, or test trees."""
    skip = {"src", "target", ".git", "tests", "test", "benches", "examples", "fuzz"}
    found = set()
    for current, directories, filenames in os.walk(crate, followlinks=False):
        relative = Path(current).relative_to(crate)
        directories[:] = [
            directory
            for directory in directories
            if directory not in skip
            and len(relative.parts) < 3
            and not (Path(current) / directory).is_symlink()
        ]
        for filename in filenames:
            candidate = Path(current) / filename
            if filename == "NOTICE-SOURCES.json":
                continue
            is_notice = filename.upper().startswith(
                ("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT")
            )
            # The font licenses also use descriptive filenames, such as Hack-Regular.txt.
            is_font_notice = (
                name == "epaint_default_fonts"
                and "fonts" in relative.parts
                and candidate.suffix.lower() == ".txt"
            )
            if (is_notice or is_font_notice) and candidate.is_file() and not candidate.is_symlink():
                found.add(candidate)
    # ring's top-level LICENSE explicitly refers to these files under src/.
    if name == "ring":
        for filename in ("LICENSE-APACHE", "LICENSE-MIT"):
            candidate = crate / "src" / "polyfill" / "once_cell" / filename
            if candidate.is_file() and not candidate.is_symlink():
                found.add(candidate)
    return found


def third_party_notices(package, target):
    metadata = json.loads(command(
        "cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target
    ))
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root = metadata["resolve"]["root"]
    dependencies = set()
    pending = [root]
    while pending:
        identifier = pending.pop()
        if identifier in dependencies:
            continue
        dependencies.add(identifier)
        pending.extend(dependency["pkg"] for dependency in nodes[identifier]["deps"])

    inventory = []
    missing = []
    for dependency in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        if dependency["id"] not in dependencies or dependency["id"] == root:
            continue
        name, version = dependency["name"], dependency["version"]
        identifier = f"{name}-{version}"
        crate = Path(dependency["manifest_path"]).parent.resolve()
        declared = dependency.get("license") or "UNSPECIFIED"
        selected = selected_license(declared)
        if name == "zune-inflate" and "Zlib" in declared:
            # Its exact source revision publishes the complete Zlib license text.
            selected = "Zlib"
        collected = []
        sources = [(crate, notice_files(crate, name))]
        # Curated supplemental notices can restore files omitted from a published crate.
        supplement = ROOT / "third-party" / identifier
        if supplement.is_dir() and not supplement.is_symlink():
            sources.append((supplement, notice_files(supplement, name)))
        if dependency.get("license_file"):
            explicit = Path(dependency["license_file"])
            if not explicit.is_absolute():
                explicit = crate / explicit
            if explicit.is_file() and not explicit.is_symlink() and explicit.resolve().is_relative_to(crate):
                sources[0][1].add(explicit.resolve())
        for source_root, files in sources:
            for source in sorted(files):
                relative = source.relative_to(source_root)
                destination = package / "licenses" / identifier / relative
                # Keep the published crate's original text if an overlay uses the same name.
                if not destination.exists():
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(source, destination)
                collected.append(destination.relative_to(package).as_posix())

        provenance = supplement / "NOTICE-SOURCES.json"
        if provenance.is_file() and not provenance.is_symlink():
            records = json.loads(provenance.read_text(encoding="utf-8"))
            for record in records:
                original = (supplement / record["filename"]).resolve()
                if not original.is_relative_to(supplement.resolve()) or sha256(original) != record["sha256"]:
                    raise ValueError(f"invalid supplemental notice provenance for {identifier}")
                destination = package / "licenses" / identifier / record["filename"]
                destination.parent.mkdir(parents=True, exist_ok=True)
                if not destination.exists():
                    shutil.copy2(original, destination)
            destination = package / "licenses" / identifier / provenance.name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(provenance, destination)

        source_url = dependency.get("source")
        download_url = None
        if source_url == "registry+https://github.com/rust-lang/crates.io-index":
            download_url = f"https://crates.io/api/v1/crates/{name}/{version}/download"
        entry = {
            "name": name,
            "version": version,
            "declared_license_expression": declared,
            "selected_license_expression": selected,
            "repository": dependency.get("repository"),
            "cargo_source": source_url,
            "versioned_source_download": download_url,
            "notice_files": sorted(set(collected)),
        }
        vcs_info = crate / ".cargo_vcs_info.json"
        if vcs_info.is_file():
            entry["published_source_revision"] = json.loads(vcs_info.read_text(encoding="utf-8"))
        if selected == "MPL-2.0":
            source_destination = package / "third-party-source" / identifier
            if any(path.is_symlink() for path in crate.rglob("*")):
                raise ValueError(f"cannot safely bundle symlinked source for {identifier}")
            shutil.copytree(crate, source_destination, ignore=shutil.ignore_patterns("target", ".git"))
            entry["bundled_source"] = source_destination.relative_to(package).as_posix()
        if not collected or declared == "UNSPECIFIED":
            missing.append(f"{identifier}: declared={declared}; source={download_url or source_url}")
            entry["notice_status"] = "MISSING: no license/notice document in crate or supplement"
        else:
            entry["notice_status"] = "collected"
        inventory.append(entry)

    (package / "THIRD-PARTY.json").write_text(
        json.dumps(inventory, indent=2) + "\n", encoding="utf-8"
    )
    (package / "MISSING-LICENSES.txt").write_text(
        ("\n".join(missing) if missing else "No missing license/notice documents.") + "\n",
        encoding="utf-8",
    )
    print(f"Third-party dependencies: {len(inventory)}; missing notice documents: {len(missing)}")
    for notice in missing:
        print(f"MISSING LICENSE: {notice}")
    return len(missing)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--runner", required=True)
    args = parser.parse_args()

    rustc = command("rustc", "--version", "--verbose")
    host = next(line[6:] for line in rustc.splitlines() if line.startswith("host: "))
    if host != args.target:
        parser.error(f"native packaging requires host {args.target}, found {host}")
    commit = command("git", "rev-parse", "HEAD")
    if os.environ.get("SOURCE_SHA", commit) != commit:
        parser.error("checked-out commit differs from SOURCE_SHA")

    binary_name = "ironvnc.exe" if "windows" in args.target else "ironvnc"
    binary = ROOT / "target" / args.target / "release" / binary_name
    smoke = subprocess.run(
        [str(binary), "--help"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if "--host" not in smoke.stdout or "Usage:" not in smoke.stdout:
        parser.error("--help did not print the expected CLI usage")
    print(smoke.stdout)

    package_name = f"ironvnc-{args.target}-{commit[:12]}"
    output = ROOT / "dist"
    output.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ironvnc-package-") as temporary:
        package = Path(temporary) / package_name
        package.mkdir()
        shutil.copy2(binary, package / binary_name)
        for filename in ("README.md", "LICENSE-APACHE", "LICENSE-MIT", "Cargo.lock"):
            shutil.copy2(ROOT / filename, package / filename)
        # Include current protocol/support documentation when present.
        docs = ROOT / "docs"
        if docs.is_dir():
            for document in sorted(docs.rglob("*.md")):
                destination = package / document.relative_to(ROOT)
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(document, destination)

        missing_notices = third_party_notices(package, args.target)
        if missing_notices:
            raise ValueError("complete third-party notice documents before packaging binaries")

        run_url = None
        if os.environ.get("GITHUB_RUN_ID"):
            run_url = (
                f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/"
                f"{os.environ['GITHUB_REPOSITORY']}/actions/runs/"
                f"{os.environ['GITHUB_RUN_ID']}"
            )
        manifest = {
            "source_commit": commit,
            "source_repository": os.environ.get("GITHUB_REPOSITORY"),
            "target": args.target,
            "rust_host": host,
            "runner_label": args.runner,
            "runner_architecture": os.environ.get("RUNNER_ARCH"),
            "runner_os": platform.platform(),
            "rustc": rustc,
            "cargo": command("cargo", "--version"),
            "binary_sha256": sha256(package / binary_name),
            "verification": ["native binary --help"],
            "prior_ci_verification": os.environ.get("CI_VERIFICATION_COMPLETED", "").split(",")
            if os.environ.get("CI_VERIFICATION_COMPLETED") else [],
            "missing_third_party_notice_documents": missing_notices,
            "verification_limits": "No GUI, desktop capture, or private server test in CI.",
            "workflow_run": run_url,
        }
        (package / "BUILD-INFO.json").write_text(
            json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
        )
        files = sorted(path for path in package.rglob("*") if path.is_file())
        checksums = "".join(
            f"{sha256(path)}  {path.relative_to(package).as_posix()}\n" for path in files
        )
        (package / "SHA256SUMS").write_text(checksums, encoding="utf-8")

        if "windows" in args.target:
            archive = output / f"{package_name}.zip"
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as target:
                for path in sorted(package.rglob("*")):
                    if path.is_file():
                        target.write(path, path.relative_to(package.parent).as_posix())
        else:
            # tar preserves the executable bit, unlike a raw Actions upload.
            archive = output / f"{package_name}.tar.gz"
            with tarfile.open(archive, "w:gz") as target:
                target.add(package, arcname=package_name)

    checksum = output / f"{archive.name}.sha256"
    checksum.write_text(f"{sha256(archive)}  {archive.name}\n", encoding="utf-8")
    print(f"Created {archive.name}\n{checksum.read_text(encoding='utf-8').strip()}")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
            summary.write(
                f"### Native binary: `{args.target}`\n\n"
                f"Source commit: `{commit}`  \nRunner: `{args.runner}`  \n"
                f"Prior CI checks: `{os.environ.get('CI_VERIFICATION_COMPLETED', 'not recorded')}`.  \n"
                "Package check: native binary `--help`.  \n"
                f"Archive: `{archive.name}`  \nSHA256: `{sha256(archive)}`\n\n"
                f"Third-party notice documents missing: {missing_notices}.  \n"
                "GUI, desktop capture, and private server interoperability are not tested.\n"
            )


if __name__ == "__main__":
    main()
