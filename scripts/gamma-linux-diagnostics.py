# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""One-off bounded CI observation; never execute on a shared development host."""

import collections
import datetime
import errno
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import urllib.request


CGROUP_FILES = (
    "memory.max", "memory.high", "memory.current", "memory.peak", "memory.oom.group", "memory.swap.max",
    "memory.swap.current", "memory.events", "memory.events.local", "memory.stat",
    "pids.max", "pids.current", "pids.events",
)
JOURNALS = ("gamma-progress.log", "gamma-selection.jsonl", "testing-verdicts.jsonl", "testing-phase.json")
MUTANT_FIELDS = ("id", "ordinal", "file", "package", "line", "end_line", "column", "mutator", "item_path", "original", "replacement", "outcome")


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def read(path, binary=False):
    try:
        return path.read_bytes() if binary else path.read_text().strip()
    except (FileNotFoundError, ProcessLookupError):
        return b"" if binary else None
    except PermissionError as error:
        print(f"{now()} unavailable {path}: {type(error).__name__}", flush=True)
        return b"" if binary else None


def cgroup(path):
    return {name: read(path / name) for name in CGROUP_FILES}


def own_group():
    entry = next(line for line in Path("/proc/self/cgroup").read_text().splitlines() if line.startswith("0::"))
    return Path("/sys/fs/cgroup") / entry[3:].lstrip("/")


def run(output):
    own = own_group()
    boundary = os.environ["GAMMA_DIAGNOSTIC_BOUNDARY"]
    group = next(path for path in (own, *own.parents) if path.name == boundary)
    limits = cgroup(group)
    if (limits["memory.max"] != str(10 * 1024**3) or limits["memory.high"] != str(7 * 1024**3)
            or limits["memory.swap.max"] != "0"
            or limits["pids.max"] != "1024" or limits["memory.oom.group"] != "1"):
        raise RuntimeError(f"Refusing a run without the expected external limits: {limits}")
    if limits["memory.events.local"] is None or limits["pids.events"] is None:
        raise RuntimeError(f"Refusing a run without resource-limit counters: {limits}")
    (output / "verified-limits.json").write_text(json.dumps({
        "time": now(), "cgroup": str(group), "limits": limits,
        "cargoBuildJobs": os.environ["CARGO_BUILD_JOBS"],
    }, indent=2))
    print(f"Verified independent limits: {json.dumps(limits)}", flush=True)
    print(f"Cargo compilation parallelism: {os.environ['CARGO_BUILD_JOBS']}", flush=True)
    os.execvp("just", ["just", "anvil-scheduled-exhaustive"])


def sample(unit):
    processes = []
    groups = {Path("/sys/fs/cgroup/user.slice") / unit}
    process_count = 0
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        status = read(entry / "status")
        if status is None:
            continue
        fields = dict(line.split(":\t", 1) for line in status.splitlines() if ":\t" in line)
        if "VmRSS" not in fields:
            continue
        process_count += 1
        group_text = read(entry / "cgroup")
        if group_text is None:
            continue
        name = fields.get("Name", "")
        if unit not in group_text and not any(part in name.lower() for part in ("runner.", "cargo-gamma")):
            continue
        active = None
        if unit in group_text:
            environment = read(entry / "environ", binary=True)
            active = next((value.split(b"=", 1)[1].decode("ascii", errors="replace")
                           for value in environment.split(b"\0") if value.startswith(b"GAMMA_ACTIVE=")), None)
            for line in group_text.splitlines():
                if line.startswith("0::"):
                    group = Path("/sys/fs/cgroup") / line[3:].lstrip("/")
                    while unit in str(group):
                        groups.add(group)
                        group = group.parent
        processes.append({
            "pid": int(entry.name), "ppid": int(fields["PPid"]), "name": name,
            "rssKiB": int(fields["VmRSS"].split()[0]), "threads": int(fields["Threads"]),
            "activeOrdinal": active, "cgroup": group_text, "umask": fields.get("Umask"),
        })
    counters = {}
    for path in sorted(groups):
        try:
            counters[str(path)] = cgroup(path)
        except OSError as error:
            if error.errno != errno.ENODEV or path.name == unit:
                raise
            print(f"{now()} transient test cgroup disappeared while sampling: {path}", flush=True)
    memory = dict(line.split(":", 1) for line in Path("/proc/meminfo").read_text().splitlines())
    return {
        "time": now(), "processCount": process_count,
        "availableKiB": int(memory["MemAvailable"].split()[0]),
        "swapFreeKiB": int(memory["SwapFree"].split()[0]),
        "processes": processes,
        "cgroups": counters,
    }


def api(method, route, payload):
    request = urllib.request.Request(
        f"{os.environ['GITHUB_API_URL']}/repos/{os.environ['GITHUB_REPOSITORY']}/{route}",
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": "Bearer " + os.environ["GH_TOKEN"],
            "Accept": "application/vnd.github+json",
            "Content-Type": "application/json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        method=method,
    )
    with urllib.request.urlopen(request, timeout=20) as response:
        return json.load(response)


def tail(path, count=10):
    if not path.exists():
        return "(not created yet)"
    with path.open("rb") as stream:
        stream.seek(max(0, path.stat().st_size - 10000))
        return "\n".join(stream.read().decode("utf-8", errors="replace").splitlines()[-count:])


def preserve(output, offsets, inventories):
    cache = Path("target/cargo-gamma/cache")
    if not cache.exists():
        return
    for base in cache.iterdir():
        if not base.is_dir():
            continue
        destination = output / "journals" / base.name
        for name in JOURNALS:
            path = base / name
            if not path.is_file():
                continue
            destination.mkdir(parents=True, exist_ok=True)
            if name == "testing-phase.json":
                (destination / name).write_bytes(path.read_bytes())
                continue
            offset = offsets.get(path, 0)
            with path.open("rb") as source:
                if path.stat().st_size < offset:
                    raise RuntimeError(f"Journal unexpectedly truncated: {path}")
                source.seek(offset)
                chunk = source.read(8 * 1024 * 1024)
            if chunk:
                with (destination / name).open("ab") as target:
                    target.write(chunk)
                offsets[path] = offset + len(chunk)
        snapshot = base / "testing-session.json"
        if base not in inventories and snapshot.is_file():
            # Do not publish the saved workspace/environment: retain only mutation identities.
            state = json.loads(snapshot.read_bytes())
            inventory = [{key: mutant.get(key) for key in MUTANT_FIELDS} for mutant in state["plan"]["mutants"]]
            destination.mkdir(parents=True, exist_ok=True)
            (destination / "mutation-inventory.json").write_text(json.dumps(inventory))
            inventories.add(base)


def observe(output, unit):
    output.mkdir(parents=True, exist_ok=True)
    offsets = {}
    inventories = set()
    history = collections.deque(maxlen=12)
    check = api("POST", "check-runs", {
        "name": "Protected Linux full exhaustive checks",
        "head_sha": os.environ["GITHUB_SHA"], "status": "in_progress",
        "external_id": f"{os.environ['GITHUB_RUN_ID']}/{os.environ['GITHUB_RUN_ATTEMPT']}",
        "details_url": f"{os.environ['GITHUB_SERVER_URL']}/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}",
    })
    published = 0
    try:
        while True:
            state = sample(unit)
            with (output / "processes.jsonl").open("a") as stream:
                stream.write(json.dumps(state) + "\n")
            preserve(output, offsets, inventories)
            history.append({key: state[key] for key in ("time", "processCount", "availableKiB", "swapFreeKiB")})
            stopped = output / "controlled-stop.json"
            for path, limits in state["cgroups"].items():
                if Path(path).name != unit or stopped.exists():
                    continue
                if any(limits[name] is None for name in ("memory.current", "pids.current", "memory.events.local", "pids.events")):
                    raise RuntimeError(f"External safety counters became unavailable: {path}: {limits}")
                memory = int(limits["memory.current"] or "0")
                tasks = int(limits["pids.current"] or "0")
                events = dict(line.split() for line in (limits["memory.events.local"] or "").splitlines())
                pids = dict(line.split() for line in (limits["pids.events"] or "").splitlines())
                limit_activity = any(int(events.get(key, 0)) > 0 for key in ("max", "oom", "oom_kill"))
                if memory >= 9 * 1024**3 or tasks >= 768 or limit_activity or int(pids.get("max", 0)) > 0:
                    stopped.write_text(json.dumps({"time": now(), "cgroup": path, "limits": limits}, indent=2))
                    print(f"{now()} stopping at the external safety threshold: {limits}", flush=True)
                    subprocess.run(["systemctl", "stop", unit], check=True, timeout=30)
            done = (output / "done").exists()
            if time.monotonic() - published >= 30 or done:
                active = [process for process in state["processes"] if process["activeOrdinal"] is not None]
                text = {
                    "recentResources": list(history),
                    "activeProcesses": active[:30],
                    "activeProcessesTruncated": max(0, len(active) - 30),
                    "verifiedLimits": tail(output / "verified-limits.json", 30),
                    "recentVerdicts": {str(path.relative_to(output)): tail(path, 3)
                                       for path in output.glob("journals/*/gamma-progress.log")},
                    "groupOutput": tail(output / "group.log"),
                    "controlledStop": tail(stopped, 30),
                }
                payload = {"output": {
                    "title": f"Last protected heartbeat: {state['time']}",
                    "summary": "\U0001f916 Full-workspace mutation, feature-powerset and benchmark-compilation verification with early reclaim at 7 GiB, external 10 GiB memory, zero swap and 1024-task limits. "
                               "A limit-triggered stop is not a successful exhaustive check. "
                               "Only GAMMA_ACTIVE is inspected from child environments; saved workspace environments are not published.",
                    "text": "```json\n" + json.dumps(text, indent=2).replace("```", "~~~") + "\n```",
                }}
                if done:
                    success = (output / "done").read_text().strip() == "success" and not stopped.exists()
                    payload.update(status="completed", conclusion="success" if success else "failure")
                api("PATCH", f"check-runs/{check['id']}", payload)
                published = time.monotonic()
                (output / "ready").write_text(str(check["id"]))
                print(f"{state['time']} published {check['html_url']}", flush=True)
            if done:
                kernel = subprocess.run(["sudo", "-n", "dmesg", "--time-format=iso"], capture_output=True, text=True, timeout=10)
                (output / "kernel.log").write_text(kernel.stdout + kernel.stderr)
                (output / "finished").touch()
                return
            time.sleep(2)
    finally:
        if not (output / "finished").exists():
            print("Observer failed; stopping the diagnostic user slice rather than continuing unobserved.", flush=True)
            subprocess.run(["systemctl", "stop", unit], check=False, timeout=30)


if __name__ == "__main__":
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted":
        raise RuntimeError("This temporary diagnostic harness is restricted to disposable GitHub-hosted CI runners.")
    mode, directory, *arguments = sys.argv[1:]
    if mode == "run":
        run(Path(directory))
    elif mode == "observe":
        observe(Path(directory), arguments[0])
    else:
        raise ValueError(f"Unknown diagnostic mode: {mode}")
