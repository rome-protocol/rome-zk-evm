#!/usr/bin/env python3
"""One Agave crate set, written once.

The ONE consistent set of solana-*, spl-* and agave-* crates lives in the root Cargo.toml's
[workspace.dependencies]. Every other manifest must inherit it with `workspace = true`.

This reads every tracked Cargo.toml as TOML (not by grepping lines), so it sees every shape a pin can
take: the inline form, the table form ([dependencies.solana-x] + version = "..."), a renamed import
(foo = { package = "solana-x", version = ... }), and the same under dev-dependencies,
build-dependencies and every [target.<cfg>.*] table.

Two kinds of finding, each refused by name:

  LiteralSolanaPin <file>:<dep>
      An in-workspace manifest carries its own version for a solana-*/spl-*/agave-* crate instead of
      `workspace = true`.

  ExcludedCrateOutOfLockstep <file>:<dep>
      A crate that sits outside the workspace (so it cannot inherit) pins a Solana crate at a version
      that differs from [workspace.dependencies]. These keep their own Cargo.lock and their pins are
      kept by hand, so this is what stops them drifting.

Usage: check_workspace_deps.py [--root DIR]     (default: the repo root; files come from `git ls-files`)
"""
import argparse
import re
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11: the same parser under its old name
    import tomli as tomllib

# Crates that are not members of the root workspace (their own [workspace] table or a workspace
# `exclude`), so they cannot say `workspace = true`. They must match the root's versions instead.
STANDALONE_PREFIXES = (
    "crates/rome-zk-prover-input/",
    "crates/rome-zk-prover-input-cross-repo-wire/",
    "crates/rome-zk-prover/",
    # ZisK guest measurement lane: its own [workspace] table.
    "guest/rome-zk-bench-decode/",
)

# Not checked at all. The test-only stub bridge has its own [workspace] table and still builds on the
# old solana-program 2.1.6: it is a small entrypoint, never deployed, and loaded only by the
# zk-settlement program-test suites. Moving it to the workspace line is a separate decision, not
# something this check should silently paper over - so it is named here.
UNCHECKED_PREFIXES = ("programs/zk-settlement/tests/fixtures/stub-bridge/",)

SOLANA_NAME = re.compile(r"^(solana|spl|agave)-")
DEP_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")


def dep_entries(table, where=""):
    """Yield (section, key, spec) for every dependency table, including target.<cfg>.* ones."""
    for sec in DEP_SECTIONS:
        for key, spec in table.get(sec, {}).items():
            yield where + sec, key, spec
    for cfg, sub in table.get("target", {}).items():
        yield from dep_entries(sub, f"target.{cfg}.")


def crate_name(key, spec):
    if isinstance(spec, dict) and "package" in spec:
        return spec["package"]
    return key


def is_solana(key, spec):
    return bool(SOLANA_NAME.match(key)) or bool(SOLANA_NAME.match(crate_name(key, spec)))


def version_of(spec):
    if isinstance(spec, str):
        return spec
    if isinstance(spec, dict):
        return spec.get("version")
    return None


def norm(v):
    return v.strip().lstrip("=").strip() if isinstance(v, str) else v


def tracked_manifests(root):
    out = subprocess.check_output(["git", "ls-files", "-z", "*Cargo.toml"], cwd=root)
    return sorted(p for p in out.decode().split("\0") if p)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=str(Path(__file__).resolve().parent.parent))
    args = ap.parse_args()
    root = Path(args.root)

    manifests = tracked_manifests(root)
    if "Cargo.toml" not in manifests:
        print("check-workspace-deps: no root Cargo.toml found", file=sys.stderr)
        return 2

    with open(root / "Cargo.toml", "rb") as fh:
        root_toml = tomllib.load(fh)
    ws_versions = {}
    for key, spec in root_toml.get("workspace", {}).get("dependencies", {}).items():
        ws_versions[crate_name(key, spec)] = norm(version_of(spec))
    if not ws_versions:
        print("check-workspace-deps: root Cargo.toml has no [workspace.dependencies]", file=sys.stderr)
        return 2

    problems = []
    for f in manifests:
        with open(root / f, "rb") as fh:
            doc = tomllib.load(fh)
        if f.startswith(UNCHECKED_PREFIXES):
            continue
        standalone = f.startswith(STANDALONE_PREFIXES)
        for section, key, spec in dep_entries(doc):
            if not is_solana(key, spec):
                continue
            inherits = isinstance(spec, dict) and spec.get("workspace") is True
            name = crate_name(key, spec)
            if standalone:
                want = ws_versions.get(name)
                have = norm(version_of(spec))
                if want is None:
                    problems.append(
                        f"ExcludedCrateOutOfLockstep {f}:{key} [{section}] {name} has no entry in "
                        f"[workspace.dependencies] to match"
                    )
                elif have != want:
                    problems.append(
                        f"ExcludedCrateOutOfLockstep {f}:{key} [{section}] pins {name} {have!r}, "
                        f"[workspace.dependencies] has {want!r}"
                    )
            elif not inherits:
                problems.append(
                    f"LiteralSolanaPin {f}:{key} [{section}] {name} must be `workspace = true`"
                )

    if problems:
        for p in problems:
            print(p, file=sys.stderr)
        print(
            "check-workspace-deps: the Solana crate set has drifted from [workspace.dependencies] "
            "- see the lines above",
            file=sys.stderr,
        )
        return 1
    print(
        "check-workspace-deps: every in-workspace manifest inherits its Solana crates, and every "
        "standalone crate matches [workspace.dependencies]"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
