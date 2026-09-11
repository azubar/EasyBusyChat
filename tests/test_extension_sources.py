"""Offline structural checks; platform applicability is checked by build_releases.py."""
from pathlib import Path
import hashlib
import struct
import unittest
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
FAMILIES = ("EasyBusyChat", "EasyBusyChatUniversal")
NS = {"md": "http://v8.1c.ru/8.3/MDClasses"}


class ExtensionSources(unittest.TestCase):
    def test_metadata_is_well_formed(self):
        for family in FAMILIES:
            directory = ROOT / "extensions" / family
            metadata = ET.parse(directory / "Configuration.xml")
            self.assertEqual(metadata.findtext("md:Configuration/md:Properties/md:Name", namespaces=NS), family)
            self.assertTrue(metadata.findtext("md:Configuration/md:Properties/md:Version", namespaces=NS))
            for path in directory.rglob("*.xml"):
                with self.subTest(path=str(path.relative_to(ROOT))):
                    ET.parse(path)

    def test_component_templates_have_correct_architecture_and_match(self):
        for template, machine in (("EasyBusyChat_x32", 0x014C), ("EasyBusyChat_x64", 0x8664)):
            hashes = []
            for family in FAMILIES:
                path = ROOT / "extensions" / family / "CommonTemplates" / template / "Ext" / "Template.bin"
                data = path.read_bytes()
                self.assertEqual(data[:2], b"MZ")
                offset = struct.unpack_from("<I", data, 60)[0]
                self.assertEqual(data[offset:offset + 4], b"PE\0\0")
                self.assertEqual(struct.unpack_from("<H", data, offset + 4)[0], machine)
                hashes.append(hashlib.sha256(data).hexdigest())
            self.assertEqual(hashes[0], hashes[1], f"{template} differs between extension families")


if __name__ == "__main__":
    unittest.main()
