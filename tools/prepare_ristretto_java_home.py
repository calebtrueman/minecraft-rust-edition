#!/usr/bin/env python3
"""Prepare a small Java 25 class library for Ristretto's adapter tests.

Ristretto 0.34 reads classes from JMOD files. It prefers lib/modules when both
that image and jmods are present, which does not work with this adapter build.
By default, this script makes an ignored scratch home with java.base.jmod and
the JDK time-zone database. An explicit module list can make a separate home
with more modules for tests that need them.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Sequence
from pathlib import Path, PurePosixPath


ROOT = Path(__file__).resolve().parents[1]
SCRATCH_ROOT = ROOT / ".local" / "ristretto-evaluation"
OUTPUT = SCRATCH_ROOT / "java25-modules-home"
FIXED_DATE = "2000-01-01T00:00:00Z"
DEFAULT_MODULES = ("java.base",)
MODULE_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*$")


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


def normalize_modules(modules: str | Sequence[str] | None) -> tuple[str, ...]:
    if modules is None:
        return DEFAULT_MODULES
    parts = modules.split(",") if isinstance(modules, str) else list(modules)
    names = tuple(part.strip() for part in parts)
    if not names or any(not name for name in names):
        raise RuntimeError("the module list must contain module names separated by commas")
    if any(MODULE_NAME.fullmatch(name) is None for name in names):
        raise RuntimeError("module names must use letters, numbers, underscores, and dots")
    if len(set(names)) != len(names):
        raise RuntimeError("the module list contains a duplicate name")
    if "java.base" not in names:
        raise RuntimeError("the module list must include java.base")
    return names


def parse_modules_argument(value: str) -> tuple[str, ...]:
    try:
        return normalize_modules(value)
    except RuntimeError as error:
        raise argparse.ArgumentTypeError(str(error)) from error


def jimage_entries(jimage: Path, modules_image: Path) -> dict[str, list[str]]:
    listing = run_tool(jimage, "list", str(modules_image))
    modules: dict[str, list[str]] = {}
    current_module = None
    for line in listing.splitlines():
        if line.startswith("Module: "):
            current_module = line.partition(": ")[2].strip()
            modules[current_module] = []
        elif current_module and line.strip():
            modules[current_module].append(line.strip())
    if not modules:
        raise RuntimeError(f"jimage did not list any modules in {modules_image}")
    return modules


def jimage_include_patterns(module_entries: dict[str, list[str]], modules: Sequence[str]) -> str:
    patterns = {"glob:**/module-info.class"}
    for module in modules:
        entries = module_entries.get(module)
        if not entries:
            raise RuntimeError(f"the Java image has no entries for {module}")
        for entry in entries:
            parent = PurePosixPath(entry).parent
            if str(parent) == ".":
                patterns.add(f"glob:**/{entry}")
            else:
                patterns.add(f"glob:**/{parent.as_posix()}/**")
    return ",".join(sorted(patterns))


def jmod_module_name(jmod: Path, jmod_file: Path) -> str:
    description = run_tool(jmod, "describe", str(jmod_file))
    lines = description.splitlines()
    first_line = lines[0] if lines else ""
    return first_line.partition("@")[0]


def required_modules(description: str) -> set[str]:
    """Read required runtime dependencies from `jmod describe`, excluding static ones."""
    required = set()
    for line in description.splitlines():
        parts = line.split()
        if not parts or parts[0] != "requires":
            continue
        if len(parts) < 2 or MODULE_NAME.fullmatch(parts[1]) is None:
            raise RuntimeError(f"invalid module dependency in JMOD description: {line}")
        if "static" not in parts[2:]:
            required.add(parts[1])
    return required


def prepare(
    java_home: Path,
    force: bool,
    modules: str | Sequence[str] | None = None,
    output: Path | None = None,
) -> None:
    java_home = java_home.expanduser().resolve()
    modules = normalize_modules(modules)
    version = version_from_release(java_home)
    major = version.split(".", maxsplit=1)[0].split("+", maxsplit=1)[0]
    if major != "25":
        raise RuntimeError(f"expected a Java 25 JDK, found {version}")

    SCRATCH_ROOT.mkdir(parents=True, exist_ok=True)
    scratch_root = SCRATCH_ROOT.resolve()
    requested_output = OUTPUT if output is None else output.expanduser()
    if requested_output.is_symlink():
        raise RuntimeError(f"scratch output must not be a symlink: {requested_output}")
    output_path = requested_output.resolve()
    if output_path.parent != scratch_root:
        raise RuntimeError(f"scratch output must stay directly under {scratch_root}")
    if (
        output_path == java_home
        or output_path in java_home.parents
        or java_home in output_path.parents
    ):
        raise RuntimeError("the Java home and scratch output must not contain each other")
    if modules != DEFAULT_MODULES and output_path == OUTPUT.resolve():
        raise RuntimeError(
            "an extended module home needs a separate --output to keep the base-only home"
        )
    if output_path.exists() and not force:
        raise RuntimeError(f"output already exists; pass --force to replace {output_path}")

    jmods_source = java_home / "jmods"
    modules_image = java_home / "lib" / "modules"
    tzdb_source = java_home / "lib" / "tzdb.dat"
    jmod = java_tool(java_home, "jmod")
    jimage = java_tool(java_home, "jimage")
    source_jmods = {name: jmods_source / f"{name}.jmod" for name in modules}
    use_existing_jmods = all(path.is_file() for path in source_jmods.values())
    image_modules: dict[str, list[str]] | None = None
    if not use_existing_jmods and modules_image.is_file():
        image_modules = jimage_entries(jimage, modules_image)
        missing = [name for name in modules if name not in image_modules]
        if missing:
            raise RuntimeError(f"Java image is missing requested modules: {', '.join(missing)}")
    elif not use_existing_jmods:
        missing = [name for name, path in source_jmods.items() if not path.is_file()]
        if jmods_source.is_dir():
            raise RuntimeError(f"Java home is missing requested JMODs: {', '.join(missing)}")
        raise RuntimeError(
            f"Java home has neither requested JMODs under {jmods_source} "
            "nor a lib/modules image to extract"
        )
    if not tzdb_source.is_file():
        raise RuntimeError(f"Java home is missing its time-zone database: {tzdb_source}")
    security_config = java_home / "conf" / "security" / "java.security"
    extended_modules = modules != DEFAULT_MODULES
    if extended_modules and not security_config.is_file():
        raise RuntimeError(f"Java home is missing its security settings: {security_config}")

    staging = Path(tempfile.mkdtemp(prefix=".prepare-java25-", dir=SCRATCH_ROOT))
    try:
        shutil.copy2(java_home / "release", staging / "release")
        output_lib = staging / "lib"
        output_lib.mkdir()
        shutil.copy2(tzdb_source, output_lib / "tzdb.dat")
        if extended_modules:
            output_security = staging / "conf" / "security"
            output_security.mkdir(parents=True)
            shutil.copy2(security_config, output_security / "java.security")
        jmods_dir = staging / "jmods"
        jmods_dir.mkdir()

        if use_existing_jmods:
            for module, source_jmod in source_jmods.items():
                if jmod_module_name(jmod, source_jmod) != module:
                    raise RuntimeError(f"JDK JMOD name does not match its file: {source_jmod}")
                shutil.copy2(source_jmod, jmods_dir / source_jmod.name)
            method = f"copied requested JMODs from the Java 25 JDK: {', '.join(modules)}"
        else:
            extracted = staging / "extracted"
            extracted.mkdir()
            include_patterns = jimage_include_patterns(image_modules, modules)
            # jimage filters resource paths, not module names. Select paths from the requested
            # modules, then drop any other module directories before rebuilding the JMODs.
            run_tool(
                jimage,
                "extract",
                "--include",
                include_patterns,
                f"--dir={extracted}",
                str(modules_image),
            )
            for child in extracted.iterdir():
                if child.is_dir() and child.name not in modules:
                    shutil.rmtree(child)
            for module in modules:
                module_dir = extracted / module
                if not (module_dir / "module-info.class").is_file():
                    raise RuntimeError(f"jimage did not extract {module}: {module_dir}")
                run_tool(
                    jmod,
                    "create",
                    "--class-path",
                    str(module_dir),
                    "--date",
                    FIXED_DATE,
                    str(jmods_dir / f"{module}.jmod"),
                )
            shutil.rmtree(extracted)
            method = f"extracted requested modules from lib/modules: {', '.join(modules)}"

        for module in modules:
            output_jmod = jmods_dir / f"{module}.jmod"
            description = run_tool(jmod, "describe", str(output_jmod))
            lines = description.splitlines()
            module_name = lines[0].partition("@")[0] if lines else ""
            if module_name != module:
                raise RuntimeError(f"prepared JMOD has the wrong module name: {output_jmod}")
            missing = sorted(required_modules(description) - set(modules))
            if missing:
                raise RuntimeError(
                    f"{module} needs missing runtime modules: {', '.join(missing)}; "
                    "include them in --modules"
                )
            if module == "java.base":
                entries = run_tool(jmod, "list", str(output_jmod))
                if "classes/java/lang/ThreadGroup.class" not in entries:
                    raise RuntimeError("the prepared java.base.jmod is missing ThreadGroup.class")

        if output_path.exists():
            backup_parent = Path(tempfile.mkdtemp(prefix=".replace-java25-", dir=SCRATCH_ROOT))
            backup = backup_parent / "previous"
            keep_backup = False
            try:
                output_path.replace(backup)
                try:
                    staging.replace(output_path)
                except BaseException:
                    try:
                        backup.replace(output_path)
                    except OSError as restore_error:
                        keep_backup = True
                        raise RuntimeError(
                            "could not restore the previous output; it remains at "
                            f"{backup}: {restore_error}"
                        ) from restore_error
                    raise
            finally:
                if backup_parent.exists() and not keep_backup:
                    shutil.rmtree(backup_parent)
        else:
            staging.replace(output_path)
        print(f"Prepared {output_path} from Java {version}; {method}.")
        print(
            f"The output contains release, {len(modules)} JMOD(s), and lib/tzdb.dat "
            "under ignored .local/."
        )
        if extended_modules:
            print("It also contains conf/security/java.security from the same Java 25 JDK.")
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
        help="replace the selected scratch output after validation succeeds",
    )
    parser.add_argument(
        "--modules",
        type=parse_modules_argument,
        default=None,
        metavar="NAME,NAME,...",
        help="comma-separated JDK modules to include (default: java.base)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="scratch home path (defaults to .local/ristretto-evaluation/java25-modules-home)",
    )
    args = parser.parse_args()
    if args.java_home is None:
        parser.error("pass --java-home or set JAVA_HOME")
    if args.modules and args.modules != DEFAULT_MODULES and args.output is None:
        parser.error("--modules requires a separate --output to keep the base-only home")
    try:
        prepare(args.java_home, args.force, args.modules, args.output)
    except (OSError, RuntimeError, argparse.ArgumentTypeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
