#!/usr/bin/env python3
"""Refuse an intra-workspace dev-dependency that carries a version.

`cargo publish` treats the two dependency kinds oppositely. A normal
dependency has its `path` stripped and NEEDS a `version` to put in its place,
which is why every entry in the root `[workspace.dependencies]` table carries
one. A dev-dependency with a version is kept in the published manifest and has
to resolve; a dev-dependency without one is dropped outright.

So a version on an intra-workspace dev-dependency is never a formality. It is
a publish-order constraint: packaging crate A now requires crate B to already
be on crates.io at that exact version. When B depends on A normally, B
publishes after A and cannot exist yet, and neither crate can go first:

    error: failed to prepare local package for uploading
    Caused by:
      failed to select a version for the requirement `shep-client = "^0.2.2"`
      candidate versions found which didn't match: 0.2.0, 0.1.34, 0.1.33, ...
      required by package `shep-macros v0.2.2`

That is not hypothetical. It stopped the 0.2.2 release after shep-core and
shep-daemon had already published, stranding shep-macros, shep-client and shep
at 0.2.0 while the two halves of the workspace sat at different versions.

The trap is `workspace = true`, which reads as inheriting a path and silently
inherits the version alongside it. Write such an entry inline instead, with a
path and no version:

    shep-client = { path = "../shep-client" }

Exits 0 when every intra-workspace dev-dependency is path-only, 1 otherwise.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def dev_dep_tables(manifest: dict) -> list[tuple[str, dict]]:
    """Every dev-dependency table in a manifest, including per-target ones.

    `[target.'cfg(unix)'.dev-dependencies]` is as much a publish-order
    constraint as the plain table, and shep-client already has one.
    """
    tables = []
    if isinstance(plain := manifest.get("dev-dependencies"), dict):
        tables.append(("dev-dependencies", plain))
    for cfg, table in (manifest.get("target") or {}).items():
        if isinstance(scoped := (table or {}).get("dev-dependencies"), dict):
            tables.append((f"target.{cfg}.dev-dependencies", scoped))
    return tables


def resolved_package(entry, dep: str, workspace_deps: dict) -> str:
    """The real package name behind a dependency key.

    Cargo lets the key alias a package through `package`, and the alias can sit
    on the entry itself or on the root table it inherits from. Membership has to
    be checked against the package rather than the key, or

        client_for_tests = { package = "shep-client", version = "0.2.2" }

    walks straight past a table keyed by real names.
    """
    if isinstance(entry, dict):
        if isinstance(named := entry.get("package"), str):
            return named
        if entry.get("workspace") is True:
            inherited = workspace_deps.get(dep)
            if isinstance(inherited, dict) and isinstance(named := inherited.get("package"), str):
                return named
    return dep


def carries_a_version(entry, name: str, workspace_deps: dict) -> str | None:
    """The version this entry would publish with, or None if it publishes clean.

    A bare string IS a version. `workspace = true` inherits whatever the root
    table declares, which for an intra-workspace crate is always a version.
    """
    if isinstance(entry, str):
        return entry
    if not isinstance(entry, dict):
        return None
    if entry.get("workspace") is True:
        inherited = workspace_deps.get(name)
        if isinstance(inherited, str):
            return inherited
        if isinstance(inherited, dict):
            return inherited.get("version")
        return None
    return entry.get("version")


def main() -> int:
    root = tomllib.loads((ROOT / "Cargo.toml").read_text())
    workspace = root["workspace"]
    workspace_deps = workspace.get("dependencies", {})

    # `publish = false` members are skipped deliberately. Nothing packages them,
    # so a version on one of their dev-dependencies constrains no release, and
    # refusing it would be a false rejection. A gate that blocks correct work is
    # worse than one that misses: `examples` is the member this covers.
    members = {}
    for member in workspace["members"]:
        path = ROOT / member / "Cargo.toml"
        manifest = tomllib.loads(path.read_text())
        if manifest["package"].get("publish") is False:
            continue
        members[manifest["package"]["name"]] = (member, manifest)

    offenders = []
    for name, (member, manifest) in sorted(members.items()):
        for table_name, table in dev_dep_tables(manifest):
            for dep, entry in table.items():
                # The root table stays keyed by `dep`, since that is how cargo
                # resolves `workspace = true`. Only membership moves.
                package = resolved_package(entry, dep, workspace_deps)
                if package not in members:
                    continue
                version = carries_a_version(entry, dep, workspace_deps)
                if version is not None:
                    shown = package if package == dep else f"{package} (as {dep})"
                    offenders.append((member, table_name, name, shown, version))

    # To stderr, like the explanation below it: stdout is block-buffered when a
    # CI runner captures it and stderr is not, so splitting the two reorders the
    # offender list after the paragraph explaining it.
    for member, table, name, shown, version in offenders:
        print(
            f"  {member}  [{table}]  {name} dev-depends on {shown} = \"{version}\"",
            file=sys.stderr,
        )

    if offenders:
        print(
            f"\n{len(offenders)} intra-workspace dev-dependency(ies) above carry a version.\n"
            "\n"
            "cargo publish keeps a dev-dependency that has a version and drops one\n"
            "that does not, so each of these makes packaging its crate require a\n"
            "release of the other that may not exist yet. Where the other crate\n"
            "depends back on this one normally, neither can publish first and the\n"
            "release deadlocks partway through, leaving the workspace split across\n"
            "two versions on crates.io.\n"
            "\n"
            "Write the entry inline with a path and no version:\n"
            "\n"
            "    shep-client = { path = \"../shep-client\" }\n"
            "\n"
            "workspace = true is the trap: it inherits the version too.",
            file=sys.stderr,
        )
        return 1

    checked = sum(len(dev_dep_tables(m)) for _, m in members.values())
    print(
        f"Every intra-workspace dev-dependency is path-only "
        f"({len(members)} members, {checked} dev-dependency table(s))."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
