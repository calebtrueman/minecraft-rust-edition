#!/usr/bin/env python3
"""Run the Java snapshot probe inside a fresh native game window.

Build the game and helper first, then set the Java/Fabric/Minecraft paths used by
``crates/rust-edition-fabric-host/tests/host_bridge.rs``. This test creates a
tiny local Fabric mod, starts the real game with an isolated home, and checks
that the Java reply hashes the exact JSON snapshot sent by that game tick.
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
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

import agent_control_e2e as control  # noqa: E402


MOD_SOURCE = r"""
package dev.rustedition.probe;

import net.fabricmc.api.ModInitializer;

public final class TestMod implements ModInitializer, SnapshotTick {
    public static native int hostCall();

    @Override
    public void onInitialize() {
        System.setProperty("probe.initialized", Integer.toString(hostCall()));
    }

    @Override
    public String onSnapshot(String snapshotJson) {
        return Integer.toString(snapshotJson.hashCode() + hostCall());
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
    "fabric_loader_jar": "RUST_EDITION_JAVA_FABRIC_LOADER_JAR",
    "minecraft_client_jar": "RUST_EDITION_JAVA_FABRIC_GAME_JAR",
    "asm": "RUST_EDITION_JAVA_FABRIC_ASM_JAR",
    "asm_analysis": "RUST_EDITION_JAVA_FABRIC_ASM_ANALYSIS_JAR",
    "asm_commons": "RUST_EDITION_JAVA_FABRIC_ASM_COMMONS_JAR",
    "asm_tree": "RUST_EDITION_JAVA_FABRIC_ASM_TREE_JAR",
    "asm_util": "RUST_EDITION_JAVA_FABRIC_ASM_UTIL_JAR",
    "mixin": "RUST_EDITION_JAVA_FABRIC_MIXIN_JAR",
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


def make_probe(work: Path, helper: Path, env: dict[str, str]) -> tuple[Path, Path]:
    paths = {
        key: absolute_env_path(env, name, directory=key == "java_home")
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
    compile_result = subprocess.run(
        [
            str(javac),
            "--release",
            "25",
            "-g:none",
            "-cp",
            str(paths["fabric_loader_jar"]),
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
        "entrypoints": {"main": ["dev.rustedition.probe.TestMod"]},
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
                "schema_version": 1,
                "java_home": str(java_home),
                "game_version": "26.3",
                "fabric_loader_jar": str(paths["fabric_loader_jar"]),
                "minecraft_client_jar": str(paths["minecraft_client_jar"]),
                "libraries": [
                    str(paths[key])
                    for key in ("asm", "asm_analysis", "asm_commons", "asm_tree", "asm_util", "mixin")
                ],
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


def wait_for_bridge_event(driver: control.Cli, since: int, timeout: float, predicate):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
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
                and isinstance(candidate.get("snapshot_json"), str)
                and bool((decoded := json.loads(candidate["snapshot_json"])).get("world_open"))
                and bool(decoded.get("player"))
            ),
        )
        snapshot_json = event["snapshot_json"]
        value = event["value"]
        snapshot = json.loads(snapshot_json)
        expected_hash = java_string_hash(snapshot_json)
        bridge_seq = int(event.get("bridge_seq", 0))
        # The mod calls the native function once at load, then once per tick.
        # Since the bridge starts at sequence one, tick N gets native call N+1.
        expected_hash = (expected_hash + bridge_seq + 1) & 0xFFFF_FFFF
        if expected_hash >= 0x8000_0000:
            expected_hash -= 0x1_0000_0000
        if int(value) != expected_hash:
            raise AssertionError(
                f"Java hash reply {value} did not match exact sent JSON hash {expected_hash} "
                f"plus native callback count for sequence {bridge_seq}"
            )
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
                and isinstance(candidate.get("snapshot_json"), str)
                and not json.loads(candidate["snapshot_json"]).get("world_open")
            ),
        )
        closed_snapshot = json.loads(closed["snapshot_json"])
        closed_seq = int(closed.get("bridge_seq", 0))
        closed_hash = java_string_hash(closed["snapshot_json"])
        closed_hash = (closed_hash + closed_seq + 1) & 0xFFFF_FFFF
        if closed_hash >= 0x8000_0000:
            closed_hash -= 0x1_0000_0000
        if closed_snapshot.get("player") is not None or closed_snapshot.get("clock") is not None:
            raise AssertionError(f"closed-world snapshot included player or clock data: {closed_snapshot}")
        if int(closed.get("value")) != closed_hash:
            raise AssertionError(f"Java callback did not hash the exact closed-world snapshot: {closed}")

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
            "closed_world_hash": closed_hash,
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
