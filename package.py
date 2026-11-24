#!/usr/bin/env python3
"""package.py — Build standard POSIX-compliant .plugin archive for coom.

Outputs exclusively dist/coom.plugin (zero .zip files, zero coommunity.plugin).
Bundles universal assets for both Claude Code and Antigravity environments,
including the comprehensive technical documentation wiki.
"""
import os
import shutil
import zipfile
from pathlib import Path

ROOT = Path(__file__).parent.resolve()
DIST_DIR = ROOT / "dist"

CORE_FILES = [
    "plugin.json",
    "mcp_config.json",
    ".mcp.json",
    ".claude-plugin/plugin.json",
    "assets/logo.svg",
    "README.md",
    "commands/coom.md",
    "skills/coom/SKILL.md",
    "rules/COOM.md",
    "web/index.html",
]

def collect_all_files():
    """Collect core files plus all wiki technical docs and resources."""
    files = list(CORE_FILES)
    
    # Include all wiki markdown files
    wiki_dir = ROOT / "wiki"
    if wiki_dir.exists():
        for p in sorted(wiki_dir.glob("*.md")):
            rel = p.relative_to(ROOT).as_posix()
            if rel not in files:
                files.append(rel)

    return sorted(files)

def clean_archives():
    """Delete any stray .zip files and legacy coommunity/coomunity plugin archives."""
    for z in ROOT.glob("*.zip"):
        print(f"Removing stray zip: {z}")
        z.unlink()
    if DIST_DIR.exists():
        for z in DIST_DIR.glob("*.zip"):
            print(f"Removing stray zip from dist: {z}")
            z.unlink()
        for legacy in ["coommunity.plugin", "coomunity.plugin"]:
            legacy_path = DIST_DIR / legacy
            if legacy_path.exists():
                print(f"Removing legacy plugin archive: {legacy_path}")
                legacy_path.unlink()

def make_plugin_archive(dest_path: Path, files_to_include):
    """Build a standard POSIX-compliant .plugin archive (zip container)."""
    dest_path.parent.mkdir(parents=True, exist_ok=True)
    if dest_path.exists():
        dest_path.unlink()

    with zipfile.ZipFile(dest_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        for rel_str in files_to_include:
            file_path = ROOT / rel_str
            if not file_path.exists():
                print(f"  [SKIP] Missing: {rel_str}")
                continue

            # Ensure pure POSIX path with forward slashes only
            arcname = rel_str.replace("\\", "/")

            data = file_path.read_bytes()
            zinfo = zipfile.ZipInfo(arcname)
            zinfo.create_system = 3  # Unix
            if arcname.endswith(".sh") or arcname.endswith(".bat"):
                zinfo.external_attr = 0o755 << 16
            else:
                zinfo.external_attr = 0o644 << 16
            zinfo.compress_type = zipfile.ZIP_DEFLATED
            zf.writestr(zinfo, data)
            print(f"  + {arcname}")

    print(f"--> Built: {dest_path.name} ({dest_path.stat().st_size:,} bytes)")

def main():
    print("=" * 60)
    print("Coom Universal Package Builder")
    print("=" * 60)
    
    clean_archives()
    
    files = collect_all_files()
    print(f"Collected {len(files)} files to bundle.")

    DIST_DIR.mkdir(parents=True, exist_ok=True)

    # Exclusively output coom.plugin
    make_plugin_archive(DIST_DIR / "coom.plugin", files)
    
    print("\nPackage build complete. Only dist/coom.plugin generated.")

if __name__ == "__main__":
    main()
