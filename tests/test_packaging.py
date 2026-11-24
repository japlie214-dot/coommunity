#!/usr/bin/env python3
"""test_packaging.py — Verify dist/coom.plugin structure, JSON manifests, and ensure no coommunity.plugin exists."""
import json
import unittest
import zipfile
from pathlib import Path

ROOT = Path(__file__).parent.parent.resolve()
DIST_DIR = ROOT / "dist"

class TestPackaging(unittest.TestCase):
    def setUp(self):
        self.coom_plugin = DIST_DIR / "coom.plugin"
        self.coommunity_plugin = DIST_DIR / "coommunity.plugin"
        self.coomunity_plugin = DIST_DIR / "coomunity.plugin"
        self.assertTrue(self.coom_plugin.exists(), "dist/coom.plugin must exist")

    def test_legacy_archives_deleted(self):
        self.assertFalse(self.coommunity_plugin.exists(), "dist/coommunity.plugin must NOT exist")
        self.assertFalse(self.coomunity_plugin.exists(), "dist/coomunity.plugin must NOT exist")

    def test_archive_contents(self):
        with zipfile.ZipFile(self.coom_plugin, "r") as zf:
            namelist = zf.namelist()
            
            # 1. Verify no backslashes (must be pure POSIX)
            for name in namelist:
                self.assertNotIn("\\", name, f"Path {name} contains backslash")

            # 2. Verify essential manifests
            self.assertIn(".claude-plugin/plugin.json", namelist)
            self.assertIn("plugin.json", namelist)
            self.assertIn("mcp_config.json", namelist)
            self.assertIn(".mcp.json", namelist)

            # 3. Verify slash commands, skills, and rules
            self.assertIn("commands/coom.md", namelist)
            self.assertIn("skills/coom/SKILL.md", namelist)
            self.assertIn("rules/COOM.md", namelist)
            self.assertIn("assets/logo.svg", namelist)

            # 4. Verify wiki suite inclusion
            self.assertIn("wiki/Home.md", namelist)
            self.assertIn("wiki/01-Getting-Started.md", namelist)
            self.assertIn("wiki/03-Runtime-Integrations-CC-Codex-AGY.md", namelist)

            # 5. Parse Claude Code manifest
            claude_manifest_data = zf.read(".claude-plugin/plugin.json")
            claude_manifest = json.loads(claude_manifest_data.decode("utf-8"))
            self.assertEqual(claude_manifest.get("name"), "coom")
            self.assertEqual(claude_manifest.get("version"), "1.3.0")
            self.assertIn("mcpServers", claude_manifest)
            self.assertIn("coom", claude_manifest["mcpServers"])

            # 6. Parse Antigravity manifest
            agy_manifest_data = zf.read("plugin.json")
            agy_manifest = json.loads(agy_manifest_data.decode("utf-8"))
            self.assertEqual(agy_manifest.get("name"), "coom")
            self.assertEqual(agy_manifest.get("version"), "1.3.0")
            self.assertEqual(claude_manifest.get("version"), agy_manifest.get("version"))

            # 7. Check skills content for pure coom
            skill_text = zf.read("skills/coom/SKILL.md").decode("utf-8")
            self.assertIn("coom", skill_text)
            self.assertNotIn("coommunity dm", skill_text)
            self.assertNotIn("coommunity post", skill_text)

    def test_zero_stray_zip_files(self):
        zip_files = list(DIST_DIR.glob("*.zip")) + list(ROOT.glob("*.zip"))
        self.assertEqual(len(zip_files), 0, f"Found unexpected zip files: {zip_files}")

if __name__ == "__main__":
    unittest.main()
