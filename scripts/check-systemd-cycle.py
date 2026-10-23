#!/usr/bin/env python3
"""Detect systemd ordering cycles among a directory of unit files.

Why this exists
---------------
A unit that is both pulled in by a target and ordered after that same target forms a cycle as soon
as anything waits on it:

    manarouter-conductor.service:  After=default.target     # ordered after ...
                                   WantedBy=default.target  # ... the target that pulls it in
    manarouter-relay.service:      After=manarouter-conductor.service
                                   Wants=manarouter-conductor.service

At boot, `default.target` wants the relay; the relay wants the conductor; the conductor waits for
`default.target` to finish. Deadlock. systemd resolves it by **deleting a start job** and logging
one line, and the deletion is invisible from outside: the unit is not failed, it is simply never
started. `systemctl is-active` says `inactive` and nothing points at the cause.

The failure only reproduces on a real boot. Starting a unit by hand does not re-evaluate the
ordering of the target that already pulled it in, so an install that looks perfect under
`systemctl --user start` can still come back dead after a reboot.

`systemd-analyze verify` does **not** catch this -- it checks unit-file syntax, not the ordering
graph. `systemd --user --test` does, but it needs a live user manager and therefore cannot run in
the installer's isolated test. This script is the portable equivalent, and it is deliberately
conservative: it models the graph the way systemd does and reports any cycle it finds.

Usage
-----
    scripts/check-systemd-cycle.py <unit-dir> [<unit-dir> ...]

Exits 0 when the graph is acyclic, 1 when a cycle is found (printing the cycle), and 2 on a usage
or parse error.
"""

from __future__ import annotations

import sys
from pathlib import Path

# Dependencies that make the target need the unit, i.e. `WantedBy=X` means X depends on the unit.
# Modelled as an edge X -> unit, because X cannot be reached until the unit starts.
REVERSE_DEPS = ("WantedBy", "RequiredBy")
# Dependencies that make the unit need the target: an edge unit -> target.
FORWARD_DEPS = ("Wants", "Requires", "BindsTo", "Requisite")
# Ordering: `After=X` means the unit waits for X (edge unit -> X); `Before=X` is the reverse.
AFTER = ("After",)
BEFORE = ("Before",)


def parse_unit(path: Path) -> dict[str, list[str]]:
    """Collect the directives this checker cares about from one unit file.

    Section-aware: a `Wants=` under `[Install]` would be unusual, but reading only the keys we name
    means a stray `After=` in a comment or a description cannot be mistaken for a dependency.
    """
    found: dict[str, list[str]] = {}
    # systemd joins a line ending in a backslash with the following line before
    # parsing, so "After=a.service \" + "b.service" is one directive. Without the
    # join a split dependency list was dropped, reporting a real cycle as OK.
    logical: list[str] = []
    pending = ""
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        stripped = raw.rstrip()
        if stripped.endswith("\\"):
            pending += stripped[:-1] + " "
            continue
        logical.append(pending + stripped)
        pending = ""
    if pending:
        logical.append(pending)
    for raw in logical:
        line = raw.strip()
        if not line or line.startswith("#") or line.startswith(";"):
            continue
        if line.startswith("[") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        if key in AFTER + BEFORE + FORWARD_DEPS + REVERSE_DEPS:
            found.setdefault(key, []).extend(value.split())
    return found


def build_graph(units: dict[str, dict[str, list[str]]]) -> dict[str, set[str]]:
    """Edges point from a unit to something it cannot start before: `u -> v` means u waits on v."""
    graph: dict[str, set[str]] = {}

    def edge(src: str, dst: str) -> None:
        graph.setdefault(src, set()).add(dst)
        graph.setdefault(dst, set())

    for name, directives in units.items():
        graph.setdefault(name, set())
        for key in AFTER:
            for target in directives.get(key, []):
                edge(name, target)
        for key in BEFORE:
            for target in directives.get(key, []):
                # `Before=X` means X waits for this unit.
                edge(target, name)
        for key in FORWARD_DEPS:
            for target in directives.get(key, []):
                edge(name, target)
        for key in REVERSE_DEPS:
            for target in directives.get(key, []):
                edge(target, name)
    return graph


def find_cycle(graph: dict[str, set[str]]) -> list[str] | None:
    """Return one cycle as a node path (`a -> b -> a`), or None when the graph is acyclic.

    Iterative depth-first search: a recursive one would risk a stack overflow on a pathological
    graph, and the whole point of this check is to be dependable on bad input.
    """
    WHITE, GREY, BLACK = 0, 1, 2
    colour: dict[str, int] = {node: WHITE for node in graph}

    for root in sorted(graph):
        if colour[root] != WHITE:
            continue
        # Each stack frame is (node, iterator over its neighbours) so the path can be rebuilt.
        stack: list[tuple[str, list[str]]] = [(root, sorted(graph[root]))]
        path: list[str] = [root]
        colour[root] = GREY
        while stack:
            node, neighbours = stack[-1]
            if not neighbours:
                colour[node] = BLACK
                stack.pop()
                path.pop()
                continue
            nxt = neighbours.pop(0)
            state = colour.get(nxt, WHITE)
            if state == GREY:
                # `nxt` is on the current path, so everything from it onward is a cycle.
                return path[path.index(nxt):] + [nxt]
            if state == WHITE:
                colour[nxt] = GREY
                stack.append((nxt, sorted(graph.get(nxt, ()))))
                path.append(nxt)
    return None


def load(dir_arg: str) -> dict[str, dict[str, list[str]]]:
    directory = Path(dir_arg)
    if not directory.is_dir():
        print(f"check-systemd-cycle: not a directory: {dir_arg}", file=sys.stderr)
        raise SystemExit(2)
    units: dict[str, dict[str, list[str]]] = {}
    for path in sorted(directory.iterdir()):
        if path.suffix not in (".service", ".target", ".socket", ".timer", ".path"):
            continue
        # A directory or dangling symlink can carry a unit-like name. Reading it used
        # to raise IsADirectoryError/FileNotFoundError, which escaped as a traceback
        # with exit 1 -- indistinguishable from "cycle found" (also exit 1).
        if not path.is_file():
            print(
                f"check-systemd-cycle: not a regular file: {path}",
                file=sys.stderr,
            )
            raise SystemExit(2)
        try:
            units[path.name] = parse_unit(path)
        except OSError as err:
            print(f"check-systemd-cycle: cannot read {path}: {err}", file=sys.stderr)
            raise SystemExit(2)
    return units


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        # Not taken from __doc__: under -OO the docstring is stripped, so indexing it
        # raised AttributeError instead of printing usage and exiting 2.
        print("usage: check-systemd-cycle.py <unit-dir> [<unit-dir> ...]", file=sys.stderr)
        return 2

    units: dict[str, dict[str, list[str]]] = {}
    for dir_arg in argv[1:]:
        units.update(load(dir_arg))

    if not units:
        print("check-systemd-cycle: no unit files found", file=sys.stderr)
        return 2

    graph = build_graph(units)
    cycle = find_cycle(graph)
    if cycle is None:
        print(f"check-systemd-cycle: OK ({len(units)} unit(s), no ordering cycle)")
        return 0

    print("check-systemd-cycle: ORDERING CYCLE FOUND", file=sys.stderr)
    print("  " + " -> ".join(cycle), file=sys.stderr)
    print(
        "  systemd resolves this by deleting a start job, so one of these units will simply "
        "never start after a reboot.",
        file=sys.stderr,
    )
    print(
        "  A unit pulled in by a target (WantedBy=/RequiredBy=) must not be ordered After= it.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
