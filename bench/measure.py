#!/usr/bin/env python3
"""Замер ресурсов десктоп-клиента (Linux): RAM (PSS) и CPU всего дерева процессов.

PSS честнее RSS: общие библиотеки делятся между процессами, а не считаются дважды.
В дерево попадают все дочерние процессы (у Tauri это WebKitWebProcess и WebKitNetworkProcess).

Примеры:
  bench/measure.py run --label feather -- target/release/feather-desktop
  bench/measure.py attach --label discord --name Discord
"""
import argparse
import json
import os
import signal
import subprocess
import time
from pathlib import Path

CLK_TCK = os.sysconf("SC_CLK_TCK")
RESULTS = Path(__file__).parent / "results"


def _stat(pid):
    with open(f"/proc/{pid}/stat") as f:
        s = f.read()
    # Имя процесса в скобках может содержать пробелы, поэтому режем по последней ')'
    rest = s.rsplit(")", 1)[1].split()
    return int(rest[1]), int(rest[11]) + int(rest[12])  # ppid, utime+stime


def _children_map():
    m = {}
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                ppid, _ = _stat(d)
                m.setdefault(ppid, []).append(int(d))
            except (OSError, IndexError, ValueError):
                pass
    return m


def tree(roots):
    m = _children_map()
    out, i = list(roots), 0
    while i < len(out):
        out += [c for c in m.get(out[i], []) if c not in out]
        i += 1
    return out


def comm(pid):
    try:
        return Path(f"/proc/{pid}/comm").read_text().strip()
    except OSError:
        return "?"


def pss_kb(pid):
    try:
        for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                return int(line.split()[1])
    except OSError:
        pass
    return 0


def cpu_ticks(pid):
    try:
        return _stat(pid)[1]
    except OSError:
        return 0


def sample(roots, warmup, duration):
    time.sleep(warmup)
    pids = tree(roots)
    t0 = {p: cpu_ticks(p) for p in pids}
    start = time.monotonic()
    ram = []
    while time.monotonic() - start < duration:
        pids = tree(roots)
        ram.append(sum(pss_kb(p) for p in pids))
        time.sleep(1)
    elapsed = time.monotonic() - start
    pids = tree(roots)
    cpu = sum(cpu_ticks(p) - t0.get(p, 0) for p in pids) / CLK_TCK / elapsed * 100
    breakdown = sorted(((comm(p), pss_kb(p)) for p in pids), key=lambda x: -x[1])
    return {
        "processes": len(pids),
        "ram_avg_mb": round(sum(ram) / len(ram) / 1024, 1),
        "ram_max_mb": round(max(ram) / 1024, 1),
        "cpu_percent_one_core": round(cpu, 2),
        "breakdown_mb": [(n, round(kb / 1024, 1)) for n, kb in breakdown],
    }


def report(label, res):
    print(f"\n== {label} ==")
    print(f"процессов:      {res['processes']}")
    print(f"RAM (PSS) сред: {res['ram_avg_mb']} МБ, макс: {res['ram_max_mb']} МБ")
    print(f"CPU:            {res['cpu_percent_one_core']} % одного ядра")
    for n, mb in res["breakdown_mb"]:
        print(f"   {mb:8.1f} МБ  {n}")
    RESULTS.mkdir(exist_ok=True)
    out = RESULTS / f"{label}-{time.strftime('%Y%m%d-%H%M%S')}.json"
    out.write_text(json.dumps({"label": label, **res}, ensure_ascii=False, indent=2))
    print(f"сохранено: {out}")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="mode", required=True)
    for name in ("run", "attach"):
        p = sub.add_parser(name)
        p.add_argument("--label", required=True)
        p.add_argument("--warmup", type=float, default=10, help="секунд до начала замера")
        p.add_argument("--duration", type=float, default=30, help="длительность замера, с")
    sub.choices["run"].add_argument("cmd", nargs=argparse.REMAINDER)
    sub.choices["attach"].add_argument("--name", required=True, help="имя процесса (comm)")
    a = ap.parse_args()

    if a.mode == "run":
        cmd = a.cmd[1:] if a.cmd[:1] == ["--"] else a.cmd
        proc = subprocess.Popen(cmd, start_new_session=True)
        try:
            report(a.label, sample([proc.pid], a.warmup, a.duration))
        finally:
            os.killpg(proc.pid, signal.SIGTERM)
    else:
        roots = [int(d) for d in os.listdir("/proc") if d.isdigit() and comm(d) == a.name]
        if not roots:
            raise SystemExit(f"процесс {a.name!r} не найден")
        report(a.label, sample(roots, a.warmup, a.duration))


if __name__ == "__main__":
    main()
