# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""Temporary Linux CI observer; publish bounded snapshots outside the failing VM."""

import collections
import datetime
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import urllib.request


def read(path):
    try:
        return Path(path).read_text().strip()
    except (FileNotFoundError, ProcessLookupError, PermissionError) as error:
        return f"unavailable: {type(error).__name__}"


def snapshot():
    processes = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        status = read(entry / "status")
        fields = dict(line.split(":\t", 1) for line in status.splitlines() if ":\t" in line)
        if "VmRSS" not in fields:
            continue
        processes.append({
            "pid": int(entry.name),
            "ppid": fields.get("PPid"),
            "name": fields.get("Name"),
            "rss_kib": int(fields["VmRSS"].split()[0]),
            "threads": fields.get("Threads"),
            "oom_score": read(entry / "oom_score"),
            "cgroup": read(entry / "cgroup"),
        })
    processes.sort(key=lambda process: process["rss_kib"], reverse=True)
    memory = dict(line.split(":", 1) for line in read("/proc/meminfo").splitlines())
    disk = shutil.disk_usage(".")
    fs = os.statvfs(".")
    kernel = subprocess.run(
        ["sudo", "-n", "dmesg", "--level=emerg,alert,crit,err,warn", "--time-format=iso"],
        capture_output=True, text=True, timeout=5, check=False,
    )
    return {
        "time": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "memory_kib": {key: int(memory[key].split()[0]) for key in
                       ("MemTotal", "MemAvailable", "SwapTotal", "SwapFree", "Cached", "Slab", "Dirty")},
        "memory_pressure": read("/proc/pressure/memory"),
        "io_pressure": read("/proc/pressure/io"),
        "load": read("/proc/loadavg"),
        "disk_free_bytes": disk.free,
        "disk_total_bytes": disk.total,
        "inodes_free": fs.f_favail,
        "oom_kill": [line for line in read("/proc/vmstat").splitlines() if line.startswith("oom_kill ")],
        "process_count": len(processes),
        "top_rss": processes[:8],
        "runner_and_gamma": [process for process in processes if
                             any(name in str(process["name"]).lower() for name in ("runner.", "cargo-gamma"))],
        "kernel": (kernel.stdout if kernel.returncode == 0 else
                   f"dmesg failed ({kernel.returncode}): {kernel.stderr}")[-10000:],
    }


def api(method, route, payload):
    request = urllib.request.Request(
        f"{os.environ['GITHUB_API_URL']}/repos/{os.environ['GITHUB_REPOSITORY']}/{route}",
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "Accept": "application/vnd.github+json",
            "Content-Type": "application/json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        method=method,
    )
    with urllib.request.urlopen(request, timeout=20) as response:
        return json.load(response)


def log_tail(path):
    if not path.exists():
        return "(Anvil group log not created yet; setup is still running.)"
    with path.open("rb") as stream:
        stream.seek(max(0, path.stat().st_size - 12000))
        text = stream.read(12000).decode("utf-8", errors="replace")
    return re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)[-8000:]


def main():
    if sys.argv[1:] == ["--snapshot"]:
        print(json.dumps(snapshot(), indent=2))
        return
    log = Path(sys.argv[1])
    ready = Path(f"{log}.ready")
    done = Path(f"{log}.done")
    history = collections.deque(maxlen=60)
    check = api("POST", "check-runs", {
        "name": "Linux exhaustive live diagnostics",
        "head_sha": os.environ["GITHUB_SHA"],
        "status": "in_progress",
        "external_id": f"{os.environ['GITHUB_RUN_ID']}/{os.environ['GITHUB_RUN_ATTEMPT']}",
        "details_url": f"{os.environ['GITHUB_SERVER_URL']}/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}",
    })
    while True:
        sample = snapshot()
        history.append(json.dumps({
            "time": sample["time"],
            "available_kib": sample["memory_kib"]["MemAvailable"],
            "swap_free_kib": sample["memory_kib"]["SwapFree"],
            "disk_free_bytes": sample["disk_free_bytes"],
            "inodes_free": sample["inodes_free"],
            "oom_kill": sample["oom_kill"],
            "load": sample["load"],
            "top_rss": [{key: process[key] for key in ("pid", "name", "rss_kib")}
                        for process in sample["top_rss"][:3]],
        }, separators=(",", ":")))
        with Path(f"{log}.samples.jsonl").open("a") as stream:
            stream.write(json.dumps(sample) + "\n")
        text = "\n".join(history)
        payload = {
            "output": {
                "title": f"Last heartbeat: {sample['time']}",
                "summary": "\U0001f916 Live resource evidence from the unchanged Linux exhaustive group. "
                           "Updates every 15 seconds. A stale heartbeat after runner loss is evidence, "
                           "not a passing result. Samples contain process names, not command lines or environment variables.",
                "text": "### Latest Anvil output\n```text\n" + log_tail(log).replace("```", "~~~") +
                        "\n```\n### Latest complete sample\n```json\n" + json.dumps(sample) +
                        "\n```\n### Recent resource samples (JSON lines; oldest first)\n```json\n" + text + "\n```",
            },
        }
        if done.exists():
            outcome = done.read_text().strip()
            payload.update(status="completed", conclusion="success" if outcome == "success" else "failure")
        api("PATCH", f"check-runs/{check['id']}", payload)
        print(f"Published {sample['time']}; check {check['html_url']}", flush=True)
        ready.write_text(str(check["id"]))
        if done.exists() and payload.get("status") == "completed":
            Path(f"{log}.finished").touch()
            return
        time.sleep(15)


if __name__ == "__main__":
    main()
