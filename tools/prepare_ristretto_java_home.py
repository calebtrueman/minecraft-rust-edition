#!/usr/bin/env python3
"""Prepare a small Java 25 class library for Ristretto's adapter tests.

Ristretto 0.34 reads classes from JMOD files. It prefers lib/modules when both
that image and jmods are present, which does not work with this adapter build.
This script makes an ignored scratch home that contains only release and
java.base.jmod. It copies that one JMOD from a full JDK when available, or
extracts java.base from lib/modules and rebuilds the JMOD with the JDK tools.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
SCRATCH_ROOT = ROOT / ".local" / "ristretto-evaluation"
OUTPUT = SCRATCH_ROOT / "java25-modules-home"
FIXED_DATE = "2000-01-01T00:00:00Z"


def java_tool(java_home: Path, name: str) -> Path:
    executable = f"{name}.exe" if os.name == "nt" else name
    return java_home / "bin" / executable


def version_from_release(java_home: Path) -> str:
    release_path = java_home / "release"
    try:
        release = release_path.read_text(encoding="utf-8")
    except OSError as error:
        raise RuntimeError(f"cannot read Java release file {release_path}: {error}") from error
    for line in release.splitlines():
        if line.startswith("JAVA_VERSION="):
            return line.partition("=")[2].strip().strip('"')
    raise RuntimeError(f"Java release file has no JAVA_VERSION line: {release_path}")


def run_tool(tool: Path, *arguments: str) -> str:
    if not tool.is_file():
        raise RuntimeError(f"Java tool is missing: {tool}")
    result = subprocess.run(
        [str(tool), *arguments],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip() or "no details"
        raise RuntimeError(f"{tool.name} failed: {detail}")
    return result.stdout


def prepare(java_home: Path, force: bool) -> None:
    java_home = java_home.expanduser().resolve()
    version = version_from_release(java_home)
    major = version.split(".", maxsplit=1)[0].split("+", maxsplit=1)[0]
    if major != "25":
        raise RuntimeError(f"expected a Java 25 JDK, found {version}")

    SCRATCH_ROOT.mkdir(parents=True, exist_ok=True)
    if OUTPUT.is_symlink():
        raise RuntimeError(f"scratch output must not be a symlink: {OUTPUT}")
    scratch_root = SCRATCH_ROOT.resolve()
    output_path = OUTPUT.resolve()
    if output_path.parent != scratch_root:
        raise RuntimeError(f"scratch output must stay directly under {scratch_root}")
    if output_path == java_home or output_path in java_home.parents or java_home in output_path.parents:
        raise RuntimeError("the Java home and scratch output must not contain each other")
    if OUTPUT.exists() and not force:
        raise RuntimeError(f"output already exists; pass --force to replace {OUTPUT}")

    jmods_source = java_home / "jmods" / "java.base.jmod"
    modules_image = java_home / "lib" / "modules"
    use_existing_jmod = jmods_source.is_file()
    if not use_existing_jmod and not modules_image.is_file():
        raise RuntimeError(
            f"Java home has neither {jmods_source} nor a lib/modules image to extract"
        )

    staging = Path(tempfile.mkdtemp(prefix=".prepare-java25-", dir=SCRATCH_ROOT))
    try:
        shutil.copy2(java_home / "release", staging / "release")
        jmods_dir = staging / "jmods"
        jmods_dir.mkdir()
        output_jmod = jmods_dir / "java.base.jmod"

        if use_existing_jmod:
            shutil.copy2(jmods_source, output_jmod)
            method = "copied java.base.jmod from the Java 25 JDK"
        else:
            extracted = staging / "extracted"
            extracted.mkdir()
            jimage = java_tool(java_home, "jimage")
            jmod = java_tool(java_home, "jmod")
            run_tool(jimage, "extract", f"--dir={extracted}", str(modules_image))
            base_module = extracted / "java.base"
            if not (base_module / "module-info.class").is_file():
                raise RuntimeError(f"jimage did not contain java.base: {base_module}")
            run_tool(
                jmod,
                "create",
                "--class-path",
                str(base_module),
                "--date",
                FIXED_DATE,
                str(output_jmod),
            )
            shutil.rmtree(extracted)
            method = "extracted java.base from lib/modules and rebuilt java.base.jmod"

        # Confirm this small home has the classes used by the adapter smoke test.
        jmod = java_tool(java_home, "jmod")
        entries = run_tool(jmod, "list", str(output_jmod))
        if "classes/java/lang/ThreadGroup.class" not in entries:
            raise RuntimeError("the prepared java.base.jmod is missing ThreadGroup.class")

        if OUTPUT.exists():
            shutil.rmtree(OUTPUT)
        staging.replace(OUTPUT)
        print(f"Prepared {OUTPUT} from Java {version}; {method}.")
        print("The output contains only a release file and java.base.jmod under ignored .local/.")
    finally:
        if staging.exists():
            shutil.rmtree(staging)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--java-home",
        type=Path,
        default=Path(os.environ["JAVA_HOME"]) if os.environ.get("JAVA_HOME") else None,
        help="Java 25 JDK home (defaults to JAVA_HOME)",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="replace the generated .local/ristretto-evaluation/java25-modules-home",
    )
    args = parser.parse_args()
    if args.java_home is None:
        parser.error("pass --java-home or set JAVA_HOME")
    try:
        prepare(args.java_home, args.force)
    except (OSError, RuntimeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
