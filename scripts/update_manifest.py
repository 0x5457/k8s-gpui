#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
import re
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import quote

TOP_LEVEL_FIELDS = {"schema", "channel", "version", "published_at", "artifacts"}
ARTIFACT_FIELDS = {"os", "arch", "name", "url", "size", "sha256"}
HEX_SHA256 = re.compile(r"[0-9a-fA-F]{64}")
CLIENT_MANIFEST_URL = "https://github.com/0x5457/k8s-gpui/releases/latest/download/update-linux-x86_64.json"
CLIENT_SIGNATURE_URL = f"{CLIENT_MANIFEST_URL}.sig"


class ProtocolError(Exception):
    pass


def parse_published_at(value):
    if value is None:
        return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
    if not isinstance(value, str) or not value.strip():
        raise ProtocolError(
            "published_at must be a non-empty ISO-8601 timestamp. "
            "Set --published-at to a value with a timezone."
        )
    text = value.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError as exc:
        raise ProtocolError("published_at is not a valid ISO-8601 timestamp.") from exc
    if parsed.tzinfo is None:
        raise ProtocolError(
            "published_at must include a timezone. Use a timestamp such as 2026-01-02T03:04:05Z."
        )
    return parsed.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def checked_repository(value):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value):
        raise ProtocolError("repository must have the form owner/name. Set --repository to owner/name.")
    return value


def checked_version(value):
    if not value or value != value.strip() or any(char in value for char in "\r\n"):
        raise ProtocolError(
            "version must be a non-empty single-line value. Set --version to a value without line breaks."
        )
    return value


def checked_tag(value, version):
    tag = checked_version(value)
    expected = f"v{version}"
    if tag != expected:
        raise ProtocolError(f"tag must be {expected}. Set --tag to {expected}.")
    return tag


def checked_component(value, label):
    if not value or value != value.strip() or any(char in value for char in "\r\n/\\"):
        raise ProtocolError(f"{label} must be a non-empty, single-line value without / or \\.")
    return value


def release_asset_url(repository, tag, name):
    return (
        f"https://github.com/{quote(repository, safe='/')}/releases/download/"
        f"{quote(tag, safe='')}/{quote(name, safe='')}"
    )


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json_atomic(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o644)
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def load_json(path):
    def reject_duplicates(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ProtocolError(f"manifest contains duplicate JSON key: {key}. Remove it.")
            result[key] = value
        return result

    try:
        with path.open("r", encoding="utf-8") as stream:
            return json.load(stream, object_pairs_hook=reject_duplicates)
    except FileNotFoundError as exc:
        raise ProtocolError(f"manifest does not exist: {path}") from exc
    except json.JSONDecodeError as exc:
        raise ProtocolError(f"manifest is not valid JSON: {exc.msg}. Check the JSON syntax.") from exc
    except UnicodeError as exc:
        raise ProtocolError("manifest is not valid UTF-8. Save the manifest as UTF-8 text.") from exc


def generate_manifest(args):
    version = checked_version(args.version)
    tag = checked_tag(args.tag, version)
    repository = checked_repository(args.repository)
    os_name = checked_component(args.os_name, "os")
    arch = checked_component(args.arch, "arch")
    published_at = parse_published_at(args.published_at)
    artifacts = []
    names = set()
    for artifact in args.artifact:
        path = artifact.expanduser()
        if not path.is_file():
            raise ProtocolError(
                f"artifact does not exist or is not a file: {artifact}. Set --artifact to an existing file."
            )
        name = path.name
        if not name or name in {".", ".."} or "/" in name or "\\" in name:
            raise ProtocolError(f"artifact has an invalid release name: {artifact}. Use a file name without path separators.")
        if name in names:
            raise ProtocolError(f"duplicate artifact name: {name}. Give each artifact a different file name.")
        names.add(name)
        size = path.stat().st_size
        if size <= 0:
            raise ProtocolError(f"artifact is empty: {artifact}. Add file content before generating the manifest.")
        artifacts.append(
            {
                "os": os_name,
                "arch": arch,
                "name": name,
                "url": release_asset_url(repository, tag, name),
                "size": size,
                "sha256": sha256_file(path),
            }
        )
    if not artifacts:
        raise ProtocolError("At least one artifact is required. Pass --artifact.")
    manifest = {
        "schema": 1,
        "channel": "stable",
        "version": version,
        "published_at": published_at,
        "artifacts": artifacts,
    }
    write_json_atomic(args.output, manifest)
    print(f"Wrote {args.output} with {len(artifacts)} artifacts")


def checked_manifest(value, repository, tag, version, os_name, arch):
    if not isinstance(value, dict) or set(value) != TOP_LEVEL_FIELDS:
        raise ProtocolError("manifest must contain exactly the protocol fields.")
    if type(value["schema"]) is not int or value["schema"] != 1:
        raise ProtocolError("manifest schema must be 1.")
    if value["channel"] != "stable":
        raise ProtocolError("manifest channel must be stable.")
    if value["version"] != version:
        raise ProtocolError("manifest version does not match --version.")
    if not isinstance(value["artifacts"], list) or not value["artifacts"]:
        raise ProtocolError("manifest artifacts must be a non-empty array.")
    parse_published_at(value["published_at"])
    names = set()
    for index, artifact in enumerate(value["artifacts"]):
        label = f"artifact {index}"
        if not isinstance(artifact, dict) or set(artifact) != ARTIFACT_FIELDS:
            raise ProtocolError(f"{label} must contain exactly the artifact fields.")
        if artifact["os"] != os_name or artifact["arch"] != arch:
            raise ProtocolError(f"{label} has an unexpected platform. Set the os and arch fields to the requested values.")
        name = artifact["name"]
        if not isinstance(name, str) or not name or name in {".", ".."} or "/" in name or "\\" in name:
            raise ProtocolError(f"{label} has an invalid name. Use a file name without / or \\.")
        if name in names:
            raise ProtocolError(f"duplicate artifact name: {name}. Give each artifact a different file name.")
        names.add(name)
        expected_url = release_asset_url(repository, tag, name)
        if artifact["url"] != expected_url:
            raise ProtocolError(f"{label} URL does not point to the release asset for tag {tag}.")
        if type(artifact["size"]) is not int or artifact["size"] <= 0:
            raise ProtocolError(f"{label} size must be a positive integer. Set size to the artifact size in bytes.")
        if not isinstance(artifact["sha256"], str) or not HEX_SHA256.fullmatch(artifact["sha256"]):
            raise ProtocolError(f"{label} sha256 must be 64 hexadecimal characters. Set the sha256 field to the artifact hash.")
    return names


def verify_manifest(args):
    version = checked_version(args.version)
    tag = checked_tag(args.tag, version)
    repository = checked_repository(args.repository)
    os_name = checked_component(args.os_name, "os")
    arch = checked_component(args.arch, "arch")
    manifest = load_json(args.manifest)
    names = checked_manifest(manifest, repository, tag, version, os_name, arch)
    required = set(args.required_artifact or [])
    for name in required:
        if name not in names:
            raise ProtocolError(f"manifest is missing required artifact: {name}. Add it to the artifacts array.")
    for artifact in manifest["artifacts"]:
        path = args.artifact_root / artifact["name"]
        if not path.is_file():
            raise ProtocolError(f"manifest artifact is missing: {path}. Check the artifact root.")
        if path.stat().st_size != artifact["size"]:
            raise ProtocolError(f"manifest size does not match: {path}. Regenerate the manifest or replace the artifact.")
        if sha256_file(path).lower() != artifact["sha256"].lower():
            raise ProtocolError(f"manifest sha256 does not match: {path}. Regenerate the manifest or replace the artifact.")
    print(f"Verified {len(manifest['artifacts'])} artifacts in {args.manifest}")


def build_parser():
    parser = argparse.ArgumentParser(
        description=(
            "Generate or verify the Linux updater manifest. "
            f"Client URL: {CLIENT_MANIFEST_URL}. Signature URL: {CLIENT_SIGNATURE_URL}."
        )
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    generate = subparsers.add_parser("generate", help="generate the update-linux manifest JSON")
    generate.add_argument("--output", required=True, type=Path)
    generate.add_argument("--repository", required=True)
    generate.add_argument("--tag", required=True)
    generate.add_argument("--version", required=True)
    generate.add_argument("--os", dest="os_name", required=True)
    generate.add_argument("--arch", required=True)
    generate.add_argument("--artifact", action="append", type=Path, required=True)
    generate.add_argument("--published-at")

    verify = subparsers.add_parser("verify", help="verify manifest fields, files, sizes, and hashes")
    verify.add_argument("--manifest", required=True, type=Path)
    verify.add_argument("--artifact-root", required=True, type=Path)
    verify.add_argument("--repository", required=True)
    verify.add_argument("--tag", required=True)
    verify.add_argument("--version", required=True)
    verify.add_argument("--os", dest="os_name", required=True)
    verify.add_argument("--arch", required=True)
    verify.add_argument("--required-artifact", action="append")
    return parser


def main():
    args = build_parser().parse_args()
    try:
        if args.command == "generate":
            generate_manifest(args)
        else:
            verify_manifest(args)
    except (OSError, ProtocolError) as exc:
        print(f"Update manifest error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
