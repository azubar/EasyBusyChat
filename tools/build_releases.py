"""Build and validate CFE files in disposable, separate 1C infobases.

No live infobase is modified. Use prepare/check before increasing the version,
then release. Requires CF or XML sources of the target BAS configuration for prepare.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
WORK = ROOT / ".local/release-build"
FAMILIES = ("EasyBusyChat", "EasyBusyChatUniversal")
NS = {"md": "http://v8.1c.ru/8.3/MDClasses"}


def version(path):
    return ET.parse(path).findtext("md:Configuration/md:Properties/md:Version", namespaces=NS)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def source_digest(directory):
    result = hashlib.sha256()
    for path in sorted(p for p in directory.rglob("*") if p.is_file()):
        data = path.read_bytes()
        if path.name == "Configuration.xml":
            # Version is intentionally increased after the checks.
            data = re.sub(rb"<Version>[^<]*</Version>", b"<Version/>" , data)
        result.update(path.relative_to(directory).as_posix().encode() + b"\0" + data)
    return result.hexdigest()


def run(platform, args, log):
    log.parent.mkdir(parents=True, exist_ok=True)
    print(log.stem, flush=True)
    command = [str(platform), *map(str, args), "/Out", str(log), "/DisableStartupDialogs", "/DisableStartupMessages"]
    result = subprocess.run(command, creationflags=subprocess.CREATE_NO_WINDOW, timeout=1800)
    output = log.read_text(encoding="utf-8-sig", errors="replace") if log.exists() else ""
    print(f"exit={result.returncode}, log={log.relative_to(ROOT)}", flush=True)
    if result.returncode:
        raise RuntimeError(output[:10000] or f"1C exit {result.returncode}")
    return output


def designer(platform, database, args, log):
    resolved = database.resolve()
    if not resolved.is_relative_to(WORK.resolve()):
        raise ValueError("Only disposable build infobases under .local/release-build are writable")
    return run(platform, ["DESIGNER", "/F", resolved, *args], log)


def prepare(platform, sources, configuration=None):
    if configuration:
        if not configuration.is_file():
            raise ValueError("Base CF does not exist")
    elif not sources or not (sources / "Configuration.xml").exists():
        raise ValueError("prepare requires --base-sources or --base-config")
    template = WORK / ("template-cf" if configuration else "template")
    if (template / "1Cv8.1CD").exists():
        raise ValueError("Template infobase already exists; inspect its logs before reusing it")
    WORK.mkdir(parents=True, exist_ok=True)
    # 1C's connection-string parser does not understand Windows argv backslash quoting.
    command = f'"{platform}" CREATEINFOBASE File="{template}" /Out "{WORK / "create-template.log"}" /DisableStartupDialogs'
    subprocess.run(command, creationflags=subprocess.CREATE_NO_WINDOW, timeout=120, check=True)
    designer(platform, template, ["/LoadCfg", configuration, "/UpdateDBCfg"] if configuration else
             ["/LoadConfigFromFiles", sources, "/UpdateDBCfg"], WORK / f"prepare-{template.name}.log")
    metadata = {
        "configuration_cf_sha256": digest(configuration),
    } if configuration else {
        "name": ET.parse(sources / "Configuration.xml").findtext("md:Configuration/md:Properties/md:Name", namespaces=NS),
        "version": version(sources / "Configuration.xml"),
        "configuration_xml_sha256": digest(sources / "Configuration.xml"),
    }
    metadata.update(platform=str(platform), template_directory=template.name)
    (WORK / "template-info.json").write_text(json.dumps(metadata, ensure_ascii=False, indent=2), encoding="utf-8")


def database_for(name):
    database = WORK / name / "database"
    if not (database / "1Cv8.1CD").exists():
        if not (WORK / "template-info.json").exists():
            raise ValueError("Prepare a clean template first")
        database.mkdir(parents=True, exist_ok=True)
        # The template Designer process has exited; no live infobase is copied.
        info = json.loads((WORK / "template-info.json").read_text(encoding="utf-8"))
        template = (WORK / info["template_directory"]).resolve()
        if not template.is_relative_to(WORK.resolve()):
            raise ValueError("Unexpected template directory")
        shutil.copy2(template / "1Cv8.1CD", database / "1Cv8.1CD")
    return database


def load_source(platform, name, database, suffix):
    designer(platform, database, ["/LoadConfigFromFiles", ROOT / "extensions" / name, "-Extension", name],
             WORK / name / f"load-{suffix}.log")


def check(platform, name):
    database = database_for(name)
    load_source(platform, name, database, "test")
    # Keep -Extension last: on 8.3.27 placing it before the check flags
    # unexpectedly checks the base configuration instead of this extension.
    designer(platform, database, ["/CheckConfig", "-ConfigLogIntegrity",
             "-IncorrectReferences", "-ThinClient", "-Server", "-WebClient", "-ExternalConnection",
             "-HandlersExistence", "-Extension", name], WORK / name / "check-config.log")
    designer(platform, database, ["/CheckCanApplyConfigurationExtensions", "-Extension", name],
             WORK / name / "check-apply.log")
    (WORK / name / "checked.json").write_text(json.dumps({
        "source_sha256_excluding_version": source_digest(ROOT / "extensions" / name),
        "checked_version": version(ROOT / "extensions" / name / "Configuration.xml"),
    }, indent=2), encoding="utf-8")


def release(platform, name):
    database = database_for(name)
    source = ROOT / "extensions" / name
    release_version = version(source / "Configuration.xml")
    if not release_version or not re.fullmatch(r"[0-9]+(?:\.[0-9]+){1,3}", release_version):
        raise ValueError(f"Invalid release version: {release_version}")
    if not (WORK / name / "checked.json").exists():
        raise ValueError("Check the extension before releasing it")
    verified = json.loads((WORK / name / "checked.json").read_text(encoding="utf-8"))
    if source_digest(source) != verified["source_sha256_excluding_version"]:
        raise ValueError("Extension sources changed since the checks; run check again")
    load_source(platform, name, database, "release")
    artifact = WORK / name / f"{name}_v{release_version}.cfe"
    designer(platform, database, ["/DumpCfg", artifact, "-Extension", name], WORK / name / "dump-cfe.log")
    if not artifact.exists() or not artifact.stat().st_size:
        raise RuntimeError("1C did not produce a non-empty CFE")
    # Reopen the actual binary and compare every non-empty BSL source, not just its name.
    designer(platform, database, ["/LoadCfg", artifact, "-Extension", name], WORK / name / "reload-cfe.log")
    exported = WORK / name / "roundtrip"
    designer(platform, database, ["/DumpConfigToFiles", exported, "-Extension", name], WORK / name / "roundtrip.log")
    if version(exported / "Configuration.xml") != release_version:
        raise RuntimeError("Version mismatch in the binary round trip")
    checked = 0
    for module in source.rglob("*.bsl"):
        expected = module.read_text(encoding="utf-8-sig").strip()
        if not expected:
            continue
        actual = exported / module.relative_to(source)
        if not actual.exists() or actual.read_text(encoding="utf-8-sig").strip() != expected:
            raise RuntimeError(f"CFE module mismatch: {module.relative_to(source)}")
        checked += 1
    binary_templates = {}
    for template in (source / "CommonTemplates").rglob("Template.bin"):
        relative = template.relative_to(source)
        actual = exported / relative
        expected_hash = digest(template)
        if not actual.exists() or digest(actual) != expected_hash:
            raise RuntimeError(f"CFE binary template mismatch: {relative}")
        binary_templates[relative.as_posix()] = {
            "bytes": template.stat().st_size, "sha256": expected_hash,
        }
    destination = ROOT / ".local" / "releases" / artifact.name
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists() and digest(destination) != digest(artifact):
        raise ValueError("A different artifact already exists at the release destination; archive it first")
    shutil.copy2(artifact, destination)
    entry = {"file": artifact.name, "version": release_version, "bytes": artifact.stat().st_size,
             "sha256": digest(artifact), "bsl_modules_verified": checked,
             "binary_templates_verified": binary_templates,
             "source_sha256_excluding_version": verified["source_sha256_excluding_version"],
             "base_configuration": json.loads((WORK / "template-info.json").read_text(encoding="utf-8"))}
    (WORK / name / "result.json").write_text(json.dumps(entry, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(entry, ensure_ascii=False), flush=True)


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("stage", choices=("prepare", "check", "release"))
    parser.add_argument("--platform", type=Path, required=True, help="Path to 1cv8.exe")
    parser.add_argument("--base-sources", type=Path)
    parser.add_argument("--base-config", type=Path)
    parser.add_argument("--only", choices=FAMILIES)
    args = parser.parse_args()
    if args.base_sources and args.base_config:
        parser.error("Choose either --base-sources or --base-config")
    if args.stage == "prepare":
        prepare(args.platform, args.base_sources.resolve() if args.base_sources else None,
                args.base_config.resolve() if args.base_config else None)
    else:
        for name in (args.only,) if args.only else FAMILIES:
            (check if args.stage == "check" else release)(args.platform, name)


if __name__ == "__main__":
    main()
