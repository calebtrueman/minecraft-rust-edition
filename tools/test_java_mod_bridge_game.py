#!/usr/bin/env python3
"""Run the Java snapshot probe inside a fresh native game window.

Build the game and helper first, then set the Java/Fabric/Minecraft paths used by
``crates/rust-edition-fabric-host/tests/host_bridge.rs``. This test creates a
tiny local Fabric mod, starts the real game with an isolated home, and checks
that the Java reply hashes each exact JSON frame and counts real client ticks.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import shutil
import subprocess
import sys
import time
import zipfile
from pathlib import Path
from pathlib import PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

import agent_control_e2e as control  # noqa: E402


MOD_SOURCE = r"""
package dev.rustedition.probe;

import net.fabricmc.api.ClientModInitializer;
import net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents;
import net.minecraft.client.Minecraft;
import com.mojang.blaze3d.platform.Window;

public final class TestMod implements ClientModInitializer, SnapshotTick {
    public static native int hostCall();
    public static native void observeWindow(Minecraft client, Window window, int width, int height);
    private static int endTickCalls;

    @Override
    public void onInitializeClient() {
        System.setProperty("probe.initialized", Integer.toString(hostCall()));
        ClientTickEvents.END_CLIENT_TICK.register(client -> {
            Window window = client.getWindow();
            observeWindow(client, window, window.getWidth(), window.getHeight());
            endTickCalls++;
        });
    }

    @Override
    public String onSnapshot(String snapshotJson) {
        return Integer.toString(snapshotJson.hashCode() + hostCall()) + ":" + endTickCalls;
    }
}
"""

SNAPSHOT_TICK = r"""
package dev.rustedition.probe;

public interface SnapshotTick {
    String onSnapshot(String snapshotJson);
}
"""

BRIDGE_PATHS = {
    "java_home": "RUST_EDITION_JAVA25_HOME",
    "minecraft_home": "RUST_EDITION_JAVA_FABRIC_MINECRAFT_HOME",
    "fabric_loader_jar": "RUST_EDITION_JAVA_FABRIC_LOADER_JAR",
    "minecraft_client_jar": "RUST_EDITION_JAVA_FABRIC_GAME_JAR",
    "fabric_api_jar": "RUST_EDITION_JAVA_FABRIC_API_JAR",
    "asm": "RUST_EDITION_JAVA_FABRIC_ASM_JAR",
    "asm_analysis": "RUST_EDITION_JAVA_FABRIC_ASM_ANALYSIS_JAR",
    "asm_commons": "RUST_EDITION_JAVA_FABRIC_ASM_COMMONS_JAR",
    "asm_tree": "RUST_EDITION_JAVA_FABRIC_ASM_TREE_JAR",
    "asm_util": "RUST_EDITION_JAVA_FABRIC_ASM_UTIL_JAR",
    "mixin": "RUST_EDITION_JAVA_FABRIC_MIXIN_JAR",
    "guava": "RUST_EDITION_JAVA_FABRIC_GUAVA_JAR",
    "failureaccess": "RUST_EDITION_JAVA_FABRIC_FAILUREACCESS_JAR",
}


def absolute_env_path(env: dict[str, str], name: str, *, directory: bool = False) -> Path:
    value = env.get(name)
    if not value:
        raise RuntimeError(f"set {name} to run this opt-in test")
    path = Path(value).resolve(strict=True)
    if directory and not path.is_dir():
        raise RuntimeError(f"{name} must point to a directory")
    if not directory and not path.is_file():
        raise RuntimeError(f"{name} must point to a file")
    return path


def java_tool(jdk: Path, name: str) -> Path:
    suffix = ".exe" if os.name == "nt" else ""
    result = jdk / "bin" / f"{name}{suffix}"
    if not result.is_file():
        raise RuntimeError(f"JAVA_HOME must include {name}")
    return result


def minecraft_runtime_libraries(minecraft_home: Path) -> list[Path]:
    manifest_path = minecraft_home / "versions" / "26.3" / "26.3.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    declarations = manifest.get("libraries")
    if not isinstance(declarations, list):
        raise RuntimeError(f"Minecraft manifest has no libraries list: {manifest_path}")

    checked: list[Path] = []
    seen: set[Path] = set()
    for library in declarations:
        coordinate = library.get("name") if isinstance(library, dict) else None
        if not isinstance(coordinate, str):
            raise RuntimeError("Minecraft library entry has no name")
        # A four-part Maven coordinate names a native classifier, not a Java JAR.
        if len(coordinate.split(":")) > 3:
            continue
        downloads = library.get("downloads")
        artifact = downloads.get("artifact") if isinstance(downloads, dict) else None
        if not isinstance(artifact, dict):
            continue
        relative_path = artifact.get("path")
        if not isinstance(relative_path, str) or "-natives-" in relative_path:
            continue
        relative = Path(relative_path)
        if (
            "\\" in relative_path
            or relative.is_absolute()
            or any(part in ("..", "") for part in relative.parts)
        ):
            raise RuntimeError(f"unsafe path in Minecraft library manifest: {relative_path}")
        path = (minecraft_home / "libraries" / relative).resolve(strict=True)
        if not path.is_file():
            raise RuntimeError(f"Minecraft Java library is not a file: {relative_path}")
        size = artifact.get("size")
        if isinstance(size, int) and path.stat().st_size != size:
            raise RuntimeError(
                f"Minecraft library {relative_path} has {path.stat().st_size} bytes, expected {size}"
            )
        expected_sha1 = artifact.get("sha1")
        if isinstance(expected_sha1, str):
            digest = hashlib.sha1()
            with path.open("rb") as source:
                for chunk in iter(lambda: source.read(64 * 1024), b""):
                    digest.update(chunk)
            if digest.hexdigest() != expected_sha1:
                raise RuntimeError(f"Minecraft library {relative_path} failed its SHA-1 check")
        if path not in seen:
            seen.add(path)
            checked.append(path)
    if not checked:
        raise RuntimeError("Minecraft 26.3 manifest has no ordinary Java libraries")
    return checked


def extract_fabric_event_jars(api_jar: Path, work: Path) -> tuple[Path, Path]:
    max_nested_jar_bytes = 64 * 1024 * 1024
    max_metadata_bytes = 1024 * 1024
    wanted = {
        "fabric-api-base": "fabric-api-base",
        "fabric-lifecycle-events-v1": "fabric-lifecycle-events-v1",
    }
    with zipfile.ZipFile(api_jar) as archive:
        entries = archive.infolist()
        by_name: dict[str, zipfile.ZipInfo] = {}
        for entry in entries:
            if entry.filename in by_name:
                raise RuntimeError(f"Fabric API archive repeats {entry.filename}")
            by_name[entry.filename] = entry
        metadata_info = by_name.get("fabric.mod.json")
        if metadata_info is None:
            raise RuntimeError("Fabric API archive has no fabric.mod.json")
        if metadata_info.file_size > max_metadata_bytes:
            raise RuntimeError("Fabric API metadata is larger than 1 MiB")
        with archive.open(metadata_info) as source:
            metadata_bytes = source.read(max_metadata_bytes + 1)
        if len(metadata_bytes) != metadata_info.file_size:
            raise RuntimeError("Fabric API metadata size did not match its ZIP entry")
        metadata = json.loads(metadata_bytes)
        declarations = metadata.get("jars", [])
        if not isinstance(declarations, list):
            raise RuntimeError("Fabric API nested-JAR metadata is not a list")
        declared = [entry.get("file") for entry in declarations if isinstance(entry, dict)]
        extracted: dict[str, Path] = {}
        for module, prefix in wanted.items():
            matches = [
                name
                for name in declared
                if isinstance(name, str)
                and PurePosixPath(name).name.startswith(prefix + "-")
                and PurePosixPath(name).name.endswith(".jar")
            ]
            if len(matches) != 1:
                raise RuntimeError(
                    f"Fabric API must declare one {module} nested JAR, found {len(matches)}"
                )
            name = matches[0]
            nested_path = PurePosixPath(name)
            raw_parts = name.split("/")
            if (
                "\\" in name
                or nested_path.is_absolute()
                or len(nested_path.parts) != 3
                or raw_parts != list(nested_path.parts)
                or nested_path.parts[:2] != ("META-INF", "jars")
                or ".." in nested_path.parts
                or "." in nested_path.parts
            ):
                raise RuntimeError(f"unexpected Fabric API nested JAR path: {name}")
            info = by_name.get(name)
            if info is None:
                raise RuntimeError(f"Fabric API does not contain declared nested JAR {name}")
            if info.file_size > max_nested_jar_bytes:
                raise RuntimeError(f"Fabric API nested JAR {name} is larger than 64 MiB")
            with archive.open(info) as source:
                nested_bytes = source.read(max_nested_jar_bytes + 1)
            if len(nested_bytes) != info.file_size:
                raise RuntimeError(f"Fabric API nested JAR {name} size did not match its ZIP entry")
            destination = work / nested_path.name
            destination.write_bytes(nested_bytes)
            extracted[module] = destination
    return extracted["fabric-api-base"], extracted["fabric-lifecycle-events-v1"]


def make_probe(work: Path, helper: Path, env: dict[str, str]) -> tuple[Path, Path]:
    paths = {
        key: absolute_env_path(env, name, directory=key in ("java_home", "minecraft_home"))
        for key, name in BRIDGE_PATHS.items()
    }
    jdk = absolute_env_path(env, "JAVA_HOME", directory=True)
    javac = java_tool(jdk, "javac")
    jar = java_tool(jdk, "jar")
    java_home = paths["java_home"]
    if not (java_home / "release").is_file():
        raise RuntimeError("RUST_EDITION_JAVA25_HOME must contain the prepared Java 25 runtime files")

    source = work / "TestMod.java"
    tick_source = work / "SnapshotTick.java"
    classes = work / "classes"
    classes.mkdir()
    source.write_text(MOD_SOURCE, encoding="utf-8")
    tick_source.write_text(SNAPSHOT_TICK, encoding="utf-8")
    fabric_api_base, fabric_lifecycle = extract_fabric_event_jars(
        paths["fabric_api_jar"], work
    )
    libraries = minecraft_runtime_libraries(paths["minecraft_home"])
    seen_libraries = set(libraries)
    for key in (
        "asm",
        "asm_analysis",
        "asm_commons",
        "asm_tree",
        "asm_util",
        "mixin",
        "guava",
        "failureaccess",
    ):
        library = paths[key]
        if library not in seen_libraries:
            seen_libraries.add(library)
            libraries.append(library)
    compile_class_path = os.pathsep.join(
        map(
            str,
            [
                paths["fabric_loader_jar"],
                paths["minecraft_client_jar"],
                *libraries,
                fabric_api_base,
                fabric_lifecycle,
            ],
        )
    )
    compile_result = subprocess.run(
        [
            str(javac),
            "--release",
            "25",
            "-g:none",
            "-cp",
            compile_class_path,
            "-d",
            str(classes),
            str(source),
            str(tick_source),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    if compile_result.returncode:
        raise RuntimeError(f"javac could not compile the probe mod:\n{compile_result.stderr}")

    metadata_path = work / "fabric.mod.json"
    metadata = {
        "schemaVersion": 1,
        "id": "probe_mod",
        "version": "1.0.0",
        "entrypoints": {"client": ["dev.rustedition.probe.TestMod"]},
        "depends": {"fabricloader": ">=0.19.5", "minecraft": "26.3"},
    }
    metadata_path.write_text(json.dumps(metadata, separators=(",", ":")), encoding="utf-8")
    mod = work / "probe-mod.jar"
    package_result = subprocess.run(
        [
            str(jar),
            "--create",
            "--file",
            str(mod),
            "-C",
            str(classes),
            ".",
            "-C",
            str(work),
            "fabric.mod.json",
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    if package_result.returncode:
        raise RuntimeError(f"jar could not package the probe mod:\n{package_result.stderr}")

    config = work / "bridge-config.json"
    config.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "java_home": str(java_home),
                "game_version": "26.3",
                "fabric_loader_jar": str(paths["fabric_loader_jar"]),
                "minecraft_client_jar": str(paths["minecraft_client_jar"]),
                "fabric_api_jar": str(paths["fabric_api_jar"]),
                "libraries": [str(path) for path in libraries],
                "mods": [str(mod)],
                "callback_mod_id": "probe_mod",
                "callback_class": "dev.rustedition.probe.TestMod",
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    if not helper.is_absolute() or not helper.is_file():
        raise RuntimeError("--helper must point to a built rust-edition-fabric-host executable")
    return mod, config


def java_string_hash(value: str) -> int:
    raw = value.encode("utf-16-be")
    result = 0
    for offset in range(0, len(raw), 2):
        unit = int.from_bytes(raw[offset : offset + 2], "big")
        result = (31 * result + unit) & 0xFFFF_FFFF
    return result - 0x1_0000_0000 if result >= 0x8000_0000 else result


def java_int(value: int) -> int:
    value &= 0xFFFF_FFFF
    return value - 0x1_0000_0000 if value >= 0x8000_0000 else value


def check_bridge_reply(event: dict[str, object], expected_end_ticks: int) -> dict[str, object]:
    if event.get("status") != "ok":
        raise AssertionError(f"Java callback failed: {event}")
    snapshot_json = event.get("snapshot_json")
    value = event.get("value")
    if not isinstance(snapshot_json, str) or not isinstance(value, str):
        raise AssertionError(f"bridge reply did not include its input and value: {event}")
    pieces = value.split(":")
    if len(pieces) != 2:
        raise AssertionError(f"Java fixture returned an unexpected value: {value!r}")
    bridge_seq = int(event["bridge_seq"])
    expected_hash_and_native_calls = java_int(java_string_hash(snapshot_json) + bridge_seq + 1)
    if int(pieces[0]) != expected_hash_and_native_calls:
        raise AssertionError(
            f"Java returned {pieces[0]}, expected exact JSON hash plus native call count "
            f"{expected_hash_and_native_calls} at bridge sequence {bridge_seq}"
        )
    actual_end_ticks = int(pieces[1])
    if actual_end_ticks != expected_end_ticks:
        raise AssertionError(
            f"Fabric END_CLIENT_TICK count was {actual_end_ticks}, expected {expected_end_ticks} "
            f"at bridge sequence {bridge_seq} ({event.get('frame_kind')}/{event.get('state_reason')})"
        )
    snapshot = json.loads(snapshot_json)
    framebuffer = snapshot.get("framebuffer")
    if not isinstance(framebuffer, dict) or int(framebuffer.get("width", 0)) <= 0 or int(
        framebuffer.get("height", 0)
    ) <= 0:
        raise AssertionError(f"frame did not include live framebuffer pixel dimensions: {snapshot}")
    return snapshot


def wait_for_bridge_event(driver: control.Cli, since: int, timeout: float, predicate):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        latest_log = driver.cwd / "logs/latest.log"
        if latest_log.is_file():
            with latest_log.open("rb") as source:
                source.seek(max(0, latest_log.stat().st_size - 64 * 1024))
                tail = source.read().decode("utf-8", errors="replace")
            failure = next(
                (line for line in reversed(tail.splitlines()) if "Java mod bridge failed:" in line),
                None,
            )
            if failure:
                raise RuntimeError(failure)
        events = driver("events", since=since)["events"]
        for event in events:
            if event.get("event") == "chat" and str(event.get("text", "")).startswith(
                "Java mod bridge failed:"
            ):
                raise RuntimeError(event["text"])
            if event.get("event") == "java_mod_reply" and event.get("status") == "failed":
                raise RuntimeError(f"Java callback failed: {event.get('value')}")
            if event.get("event") == "java_mod_reply" and predicate(event):
                return event
        time.sleep(0.1)
    raise TimeoutError(f"no matching Java mod bridge reply within {timeout:g} seconds")


def preserve_run_logs(
    args: argparse.Namespace,
    mode: str,
    work: Path,
    helper_scratch: Path,
    *,
    include_helper: bool = True,
) -> None:
    output = Path(args.out).resolve() / mode
    output.mkdir(parents=True, exist_ok=True)
    for source in (work / "control.log", work / "logs/latest.log"):
        if source.is_file():
            shutil.copy2(source, output / source.name)
    if include_helper:
        for index, root in enumerate(helper_scratch.glob(".rust-edition-mod-bridge-*")):
            for source in root.rglob("fabric.log"):
                shutil.copy2(source, output / f"helper-{index}-fabric.log")
    print(f"Saved the {mode} run logs under {output}", file=sys.stderr)


def close_game(driver: control.Cli, work: Path) -> None:
    try:
        driver("quit")
    except Exception:
        pass
    try:
        driver.close()
    except Exception:
        pass
    time.sleep(1)
    shutil.rmtree(work, ignore_errors=True)


def run(args: argparse.Namespace, mode: str) -> dict[str, object]:
    resources = Path(args.resources).resolve(strict=True)
    client = Path(args.bin_dir).resolve(strict=True) / "horizon-client"
    ctl = Path(args.bin_dir).resolve(strict=True) / "horizon-ctl"
    if os.name == "nt":
        client = client.with_suffix(".exe")
        ctl = ctl.with_suffix(".exe")
    helper = Path(args.helper).resolve(strict=True)
    for path in (client, ctl, helper):
        if not path.is_file():
            raise RuntimeError(f"built program not found: {path}")

    work, game_env = control.scratch("java-mod-bridge", resources)
    if mode == "in-frame":
        game_env["HORIZON_SERVER_THREAD"] = "0"
    else:
        game_env.pop("HORIZON_SERVER_THREAD", None)
    probe_work = work / "probe"
    probe_work.mkdir()
    mod, config = make_probe(probe_work, helper, game_env)
    helper_scratch = work / "bridge scratch"
    helper_scratch.mkdir()
    game_env.update(
        {
            "HORIZON_MOD_BRIDGE_HELPER": str(helper),
            "HORIZON_MOD_BRIDGE_CONFIG": str(config.resolve()),
            "HORIZON_MOD_BRIDGE_SCRATCH": str(helper_scratch.resolve()),
        }
    )

    socket = work / "control.sock"
    driver = control.Cli(ctl, socket, work, game_env)
    event_start = 0
    try:
        driver("launch", wait=False, offscreen=sys.platform == "darwin")
        driver("wait_for", condition="screen", value="title", timeout=180)
        initial_title = wait_for_bridge_event(
            driver,
            event_start,
            150,
            lambda candidate: candidate.get("frame_kind") == "state"
            and candidate.get("state_reason") == "title",
        )
        initial_snapshot = check_bridge_reply(initial_title, expected_end_ticks=0)
        if initial_snapshot.get("world_open") or initial_snapshot.get("player") is not None or initial_snapshot.get("clock") is not None:
            raise AssertionError(f"title state included world or player data: {initial_snapshot}")
        title_event_seq = int(initial_title["seq"])
        title_tick_events = []
        for expected_count in range(1, 4):
            title_tick = wait_for_bridge_event(
                driver,
                title_event_seq,
                30,
                lambda candidate: candidate.get("frame_kind") == "tick"
                and candidate.get("state_reason") is None
                and not json.loads(candidate["snapshot_json"]).get("world_open"),
            )
            check_bridge_reply(title_tick, expected_end_ticks=expected_count)
            title_event_seq = int(title_tick["seq"])
            title_tick_events.append(title_tick)
        driver("open", world="JavaBridgeProbe", seed="8675309", mode="creative")
        driver("wait_for", condition="loaded", timeout=180)
        spawn = driver("state")
        if not spawn.get("player") or not spawn.get("world"):
            raise AssertionError(f"game did not open the test world: {spawn}")
        spawn_x, spawn_y, spawn_z = map(float, spawn["player"]["position"])
        anchor_x, anchor_y, anchor_z = math.floor(spawn_x), math.floor(spawn_y) - 1, math.floor(spawn_z)
        driver("command", command="gamerule advance_time false")
        driver("command", command="time set 1000")
        driver(
            "command",
            command=f"fill {anchor_x - 2} {anchor_y} {anchor_z - 2} {anchor_x + 2} {anchor_y} {anchor_z + 2} minecraft:stone",
        )
        driver("command", command=f"tp @s {anchor_x + 0.5:.1f} {anchor_y + 1} {anchor_z + 0.5:.1f}")
        driver("wait", ticks=40)
        state = driver("state")
        if not state.get("player") or not state.get("world"):
            raise AssertionError(f"game did not open the test world: {state}")
        check_events = driver("events")["events"]
        for prior in check_events:
            if prior.get("event") == "chat" and str(prior.get("text", "")).startswith(
                "Java mod bridge failed:"
            ):
                raise RuntimeError(prior["text"])
        event_start = driver("events")["last"]
        event = wait_for_bridge_event(
            driver,
            event_start,
            90,
            lambda candidate: (
                candidate.get("status") == "ok"
                and candidate.get("frame_kind") == "tick"
                and candidate.get("state_reason") is None
                and isinstance(candidate.get("snapshot_json"), str)
                and bool((decoded := json.loads(candidate["snapshot_json"])).get("world_open"))
                and bool(decoded.get("player"))
            ),
        )
        snapshot_json = event["snapshot_json"]
        snapshot = check_bridge_reply(event, expected_end_ticks=int(event["bridge_seq"]) - 1)
        bridge_seq = int(event.get("bridge_seq", 0))
        expected_hash = java_int(java_string_hash(snapshot_json) + bridge_seq + 1)
        state = driver("state")
        player = snapshot["player"]
        current = state["player"]
        checks = {
            "dimension": player.get("dimension") == state.get("dimension"),
            "health": abs(float(player.get("health")) - float(current.get("health"))) < 0.001,
            "x": abs(float(player.get("x")) - float(current["position"][0])) < 1.0,
            "y": abs(float(player.get("y")) - float(current["position"][1])) < 1.0,
            "z": abs(float(player.get("z")) - float(current["position"][2])) < 1.0,
            "anchor": (
                abs(float(player.get("x")) - (anchor_x + 0.5)) < 0.25
                and abs(float(player.get("z")) - (anchor_z + 0.5)) < 0.25
                and abs(float(player.get("y")) - (anchor_y + 1)) < 0.5
            ),
            "clock": isinstance(snapshot.get("clock", {}).get("game_time"), int),
            "day_time": abs(int(snapshot["clock"]["day_time"]) - int(state["time"]["day_time"])) < 10,
            "controlled_day": int(snapshot["clock"]["day_time"]) == 1000,
        }
        if not all(checks.values()):
            raise AssertionError(f"snapshot did not line up with the native game state: {checks}; {state}; {snapshot}")

        # Return to the title screen, then confirm the same helper receives a
        # closed-world snapshot with no player or clock values.
        driver("pause")
        paused_state = driver("wait_for", condition="screen", value="paused", timeout=10)
        if paused_state.get("screen") != "paused":
            raise AssertionError(f"game did not enter its pause screen: {paused_state}")
        paused_event_start = driver("events")["last"]
        last_paused_event_seq = int(paused_event_start)
        paused_tick_events = []
        for _ in range(3):
            paused_tick = wait_for_bridge_event(
                driver,
                last_paused_event_seq,
                15,
                lambda candidate: candidate.get("frame_kind") == "tick"
                and candidate.get("state_reason") is None
                and bool(json.loads(candidate["snapshot_json"]).get("world_open")),
            )
            check_bridge_reply(paused_tick, expected_end_ticks=int(paused_tick["bridge_seq"]) - 1)
            last_paused_event_seq = int(paused_tick["seq"])
            paused_tick_events.append(paused_tick)
            if driver("state").get("screen") != "paused":
                raise AssertionError("game left the pause screen while checking paused client ticks")
        pause_buttons = control.widgets(driver)
        return_button = next(
            (
                widget
                for widget in pause_buttons.values()
                if widget.get("kind") == "button"
                and "Save and Quit" in widget.get("label", "")
            ),
            None,
        )
        if return_button is None:
            raise AssertionError(f"pause menu has no Save and Quit button: {pause_buttons}")
        leave_start = driver("events")["last"]
        driver("click", id=return_button["id"])
        driver("wait_for", condition="screen", value="title", timeout=90)
        closed = wait_for_bridge_event(
            driver,
            leave_start,
            30,
            lambda candidate: (
                candidate.get("status") == "ok"
                and candidate.get("frame_kind") == "state"
                and candidate.get("state_reason") == "world_closed"
                and isinstance(candidate.get("snapshot_json"), str)
                and not json.loads(candidate["snapshot_json"]).get("world_open")
            ),
        )
        closed_seq = int(closed.get("bridge_seq", 0))
        closed_snapshot = check_bridge_reply(closed, expected_end_ticks=closed_seq - 2)
        closed_world_hash = java_int(
            java_string_hash(closed["snapshot_json"]) + closed_seq + 1
        )
        if closed_snapshot.get("player") is not None or closed_snapshot.get("clock") is not None:
            raise AssertionError(f"closed-world snapshot included player or clock data: {closed_snapshot}")
        if closed_snapshot.get("world_open"):
            raise AssertionError(f"world-close state still reported an open world: {closed_snapshot}")
        closed_event_seq = int(closed["seq"])
        post_close_tick = wait_for_bridge_event(
            driver,
            closed_event_seq,
            15,
            lambda candidate: candidate.get("frame_kind") == "tick"
            and candidate.get("state_reason") is None
            and not json.loads(candidate["snapshot_json"]).get("world_open"),
        )
        check_bridge_reply(post_close_tick, expected_end_ticks=int(post_close_tick["bridge_seq"]) - 2)

        if not any(helper_scratch.iterdir()):
            raise AssertionError("the helper did not create its private scratch folder")
        # Save the helper log before shutdown removes its private scratch tree.
        preserve_run_logs(args, mode, work, helper_scratch)
        driver("quit")
        preserve_run_logs(args, mode, work, helper_scratch, include_helper=False)
        deadline = time.monotonic() + 5
        while any(helper_scratch.iterdir()) and time.monotonic() < deadline:
            time.sleep(0.05)
        leftovers = list(helper_scratch.iterdir())
        if leftovers:
            raise AssertionError(f"helper did not clean its private scratch folder after game exit: {leftovers}")
        return {
            "ok": True,
            "mode": mode,
            "bridge_seq": bridge_seq,
            "hash": expected_hash,
            "checks": checks,
            "world": state["world"],
            "dimension": player["dimension"],
            "closed_world_seq": closed_seq,
            "closed_world_hash": closed_world_hash,
            "title_ticks_checked": len(title_tick_events),
            "paused_ticks_checked": len(paused_tick_events),
            "test_mod_sha256": hashlib.sha256(mod.read_bytes()).hexdigest(),
        }
    except Exception:
        log_path = socket.with_suffix(".log")
        if log_path.exists():
            print("game log tail:\n" + "\n".join(log_path.read_text(errors="replace").splitlines()[-30:]), file=sys.stderr)
        preserve_run_logs(args, mode, work, helper_scratch)
        raise
    finally:
        close_game(driver, work)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, help="directory containing horizon-client and horizon-ctl")
    parser.add_argument("--helper", required=True, help="built rust-edition-fabric-host executable")
    parser.add_argument("--resources", default=str(control.lab.RESOURCES))
    parser.add_argument("--mode", choices=["both", "server-thread", "in-frame"], default="both")
    parser.add_argument(
        "--out",
        default=str(ROOT / ".local/java-mod-bridge-e2e"),
        help="where a failed run's logs are saved",
    )
    args = parser.parse_args()
    try:
        modes = ["server-thread", "in-frame"] if args.mode == "both" else [args.mode]
        results = [run(args, mode) for mode in modes]
        report = {"ok": all(result["ok"] for result in results), "runs": results}
        evidence_dir = Path(args.out).resolve()
        evidence_dir.mkdir(parents=True, exist_ok=True)
        (evidence_dir / "result.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(report, indent=2))
    except Exception as error:
        print(f"Java mod bridge game test failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
