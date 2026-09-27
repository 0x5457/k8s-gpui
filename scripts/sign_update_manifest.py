#!/usr/bin/env python3
import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

PUBLIC_KEY_ENV = "K8S_GPUI_UPDATE_PUBLIC_KEY"
PRIVATE_KEY_ENV = "K8S_GPUI_UPDATE_SIGNING_KEY"
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")
HEX_PUBLIC_KEY = re.compile(r"[0-9a-fA-F]{64}")


class SigningError(Exception):
    pass


def public_key_bytes():
    value = os.environ.get(PUBLIC_KEY_ENV)
    if value is None or not value.strip():
        raise SigningError(f"{PUBLIC_KEY_ENV} is not set. Set it to a 32-byte hexadecimal public key.")
    value = value.strip()
    if not HEX_PUBLIC_KEY.fullmatch(value):
        raise SigningError(f"{PUBLIC_KEY_ENV} must be 64 hexadecimal characters for a 32-byte public key.")
    return bytes.fromhex(value)


def private_key_bytes():
    value = os.environ.get(PRIVATE_KEY_ENV)
    if value is None or not value.strip():
        raise SigningError(f"{PRIVATE_KEY_ENV} is not set. Set it to an unencrypted PKCS#8 PEM private key.")
    value = value.encode("utf-8")
    if b"-----BEGIN PRIVATE KEY-----" not in value:
        raise SigningError(f"{PRIVATE_KEY_ENV} must contain an unencrypted PKCS#8 PEM private key.")
    if not value.endswith(b"\n"):
        value += b"\n"
    return value


def openssl(args):
    executable = shutil.which("openssl")
    if executable is None:
        raise SigningError("openssl is required. Install OpenSSL and run the command again.")
    environment = os.environ.copy()
    environment.pop(PUBLIC_KEY_ENV, None)
    environment.pop(PRIVATE_KEY_ENV, None)
    result = subprocess.run(
        [executable, *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        check=False,
    )
    if result.returncode != 0:
        raise SigningError(f"openssl {args[0]} failed. Verify the OpenSSL command and input files.")
    return result.stdout


def write_private_key(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(value)
        stream.flush()
        os.fsync(stream.fileno())
    os.chmod(path, 0o600)


def derived_public_key(private_path, expected_public):
    spki = openssl(["pkey", "-inform", "PEM", "-in", str(private_path), "-pubout", "-outform", "DER"])
    expected_spki = ED25519_SPKI_PREFIX + expected_public
    if spki != expected_spki:
        raise SigningError(
            "The private key does not match K8S_GPUI_UPDATE_PUBLIC_KEY. "
            "Set K8S_GPUI_UPDATE_PUBLIC_KEY to the matching public key."
        )
    return spki


def public_pem(spki, directory):
    der_path = directory / "public.der"
    pem_path = directory / "public.pem"
    der_path.write_bytes(spki)
    openssl(["pkey", "-pubin", "-inform", "DER", "-in", str(der_path), "-pubout", "-out", str(pem_path)])
    return pem_path


def check_key_pair():
    public = public_key_bytes()
    private = private_key_bytes()
    with tempfile.TemporaryDirectory(prefix="k8s-gpui-update-") as temporary:
        private_path = Path(temporary) / "signing-key.pem"
        write_private_key(private_path, private)
        derived_public_key(private_path, public)
    print("Updater signing key pair is valid")


def verify_signature(manifest_path, signature_path):
    if not manifest_path.is_file():
        raise SigningError(f"Manifest does not exist: {manifest_path}")
    if not signature_path.is_file():
        raise SigningError(f"Signature does not exist: {signature_path}")
    signature = signature_path.read_bytes()
    if len(signature) != 64:
        raise SigningError("Ed25519 signature must be 64 bytes. Sign the manifest again.")
    public = public_key_bytes()
    spki = ED25519_SPKI_PREFIX + public
    with tempfile.TemporaryDirectory(prefix="k8s-gpui-update-") as temporary:
        directory = Path(temporary)
        pem_path = public_pem(spki, directory)
        openssl(
            [
                "pkeyutl",
                "-rawin",
                "-verify",
                "-pubin",
                "-inkey",
                str(pem_path),
                "-sigfile",
                str(signature_path),
                "-in",
                str(manifest_path),
            ]
        )
    print(f"Verified {signature_path}")


def sign_manifest(manifest_path, signature_path):
    if not manifest_path.is_file():
        raise SigningError(f"Manifest does not exist: {manifest_path}")
    if manifest_path.resolve() == signature_path.resolve():
        raise SigningError("Manifest and signature paths must differ. Set --signature to a different path.")
    public = public_key_bytes()
    private = private_key_bytes()
    signature_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="k8s-gpui-update-") as temporary:
        directory = Path(temporary)
        private_path = directory / "signing-key.pem"
        write_private_key(private_path, private)
        spki = derived_public_key(private_path, public)
        pem_path = public_pem(spki, directory)
        temporary_signature = directory / "manifest.sig"
        openssl(
            [
                "pkeyutl",
                "-rawin",
                "-sign",
                "-inkey",
                str(private_path),
                "-in",
                str(manifest_path),
                "-out",
                str(temporary_signature),
            ]
        )
        signature = temporary_signature.read_bytes()
        if len(signature) != 64:
            raise SigningError("openssl produced an invalid Ed25519 signature. Verify the signing setup.")
        openssl(
            [
                "pkeyutl",
                "-rawin",
                "-verify",
                "-pubin",
                "-inkey",
                str(pem_path),
                "-sigfile",
                str(temporary_signature),
                "-in",
                str(manifest_path),
            ]
        )
    fd, temporary_output = tempfile.mkstemp(prefix=f".{signature_path.name}.", dir=signature_path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(signature)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary_output, 0o644)
        os.replace(temporary_output, signature_path)
    except BaseException:
        try:
            os.unlink(temporary_output)
        except FileNotFoundError:
            pass
        raise
    print(f"Wrote {signature_path}")


def build_parser():
    parser = argparse.ArgumentParser(
        description=(
            "Sign and verify the Linux updater manifest with OpenSSL Ed25519. "
            f"Set {PUBLIC_KEY_ENV} and {PRIVATE_KEY_ENV} before you run this command."
        )
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("check-key", help="verify the configured public and private key pair")
    sign = subparsers.add_parser("sign", help="sign a manifest and verify the new signature")
    sign.add_argument("--manifest", required=True, type=Path)
    sign.add_argument("--signature", required=True, type=Path)
    verify = subparsers.add_parser("verify", help="verify a manifest signature with the configured public key")
    verify.add_argument("--manifest", required=True, type=Path)
    verify.add_argument("--signature", required=True, type=Path)
    return parser


def main():
    args = build_parser().parse_args()
    try:
        if args.command == "check-key":
            check_key_pair()
        elif args.command == "sign":
            sign_manifest(args.manifest, args.signature)
        else:
            verify_signature(args.manifest, args.signature)
    except (OSError, SigningError) as exc:
        print(f"Updater signing error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
