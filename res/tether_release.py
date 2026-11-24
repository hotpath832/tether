#!/usr/bin/env python3
"""Tether update releases: make the signing key, sign an installer, write latest.json.

Tether only installs an update whose signature matches the public key built
into the app (src/tether_update.rs, PUBLIC_KEY). Keep the private key on the
build computer only, never in the repository or on the update server.

  # once: make the key pair (prints the public key to put in tether_update.rs)
  python res/tether_release.py keygen --key C:/tether-keys/update_signing_key.pem

  # per release and platform: copy the installer into the release folder,
  # sign it and add it to latest.json
  python res/tether_release.py sign --key C:/tether-keys/update_signing_key.pem \
      --file tether-1.4.9-install.exe --out release

Then publish the release folder to the update server (the URL users enter in
Settings > General > Update server): drop the files on its dashboard's Upload
tab, or

  python res/tether_release.py upload --server https://updates.example.com       --token tus_... --dir release --live

(the token comes from the dashboard's Settings tab). On a plain static web host,
just copy the folder's files there.

Needs the `cryptography` package (pip install cryptography).
"""
import argparse
import base64
import hashlib
import json
import os
import re
import shutil
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

# Must match SIGN_DOMAIN and signed_message() in src/tether_update.rs.
SIGN_DOMAIN = "tether-update-v1"


def signed_message(platform, version, file_name, sha256_hex):
    return f"{SIGN_DOMAIN}|{platform}|{version}|{file_name}|{sha256_hex}".encode()


def public_key_b64(private_key):
    raw = private_key.public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    return base64.b64encode(raw).decode()


def load_key(path):
    with open(path, "rb") as f:
        return serialization.load_pem_private_key(f.read(), password=None)


def cargo_version():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    with open(os.path.join(root, "Cargo.toml"), encoding="utf-8") as f:
        m = re.search(r'^version\s*=\s*"([^"]+)"', f.read(), re.M)
    if not m:
        sys.exit("Could not read the version from Cargo.toml; pass --version")
    return m.group(1)


def guess_platform(file_name):
    n = file_name.lower()
    if n.endswith(".exe") or n.endswith(".msi"):
        return "windows-x86_64"
    if n.endswith(".dmg"):
        return "macos-aarch64" if ("aarch64" in n or "arm64" in n) else "macos-x86_64"
    if n.endswith(".deb") or n.endswith(".rpm") or n.endswith(".appimage"):
        return "linux-aarch64" if ("aarch64" in n or "arm64" in n) else "linux-x86_64"
    sys.exit(f"Can't tell the platform of {file_name}; pass --platform")


def cmd_keygen(args):
    if os.path.exists(args.key):
        sys.exit(f"{args.key} already exists; refusing to overwrite a signing key")
    os.makedirs(os.path.dirname(os.path.abspath(args.key)), exist_ok=True)
    key = Ed25519PrivateKey.generate()
    pem = key.private_bytes(serialization.Encoding.PEM,
                            serialization.PrivateFormat.PKCS8,
                            serialization.NoEncryption())
    with open(args.key, "wb") as f:
        f.write(pem)
    print(f"Private key written to {args.key} (keep it secret, back it up).")
    print(f"Public key (PUBLIC_KEY in src/tether_update.rs): {public_key_b64(key)}")


def cmd_pubkey(args):
    print(public_key_b64(load_key(args.key)))


def cmd_sign(args):
    key = load_key(args.key)
    version = args.version or cargo_version()
    file_name = os.path.basename(args.file)
    platform = args.platform or guess_platform(file_name)
    with open(args.file, "rb") as f:
        sha = hashlib.sha256(f.read()).hexdigest()
    sig = key.sign(signed_message(platform, version, file_name, sha))

    os.makedirs(args.out, exist_ok=True)
    dest = os.path.join(args.out, file_name)
    if os.path.abspath(dest) != os.path.abspath(args.file):
        shutil.copyfile(args.file, dest)

    latest_path = os.path.join(args.out, "latest.json")
    latest = {}
    if os.path.exists(latest_path):
        with open(latest_path, encoding="utf-8") as f:
            latest = json.load(f)
        if latest.get("version") != version:
            latest = {}  # a new version replaces every platform's entry
    latest["version"] = version
    if args.notes is not None:
        latest["notes"] = args.notes
    platforms = latest.setdefault("platforms", {})
    platforms[platform] = {
        "file": file_name,
        "sha256": sha,
        "signature": base64.b64encode(sig).decode(),
    }
    with open(latest_path, "w", encoding="utf-8") as f:
        json.dump(latest, f, indent=2)
        f.write("\n")
    print(f"Signed {file_name} as {platform} {version}")
    print(f"Upload the contents of {os.path.abspath(args.out)} to the update server.")


def cmd_upload(args):
    import urllib.request
    import urllib.error
    import uuid

    names = sorted(os.listdir(args.dir))
    if "latest.json" not in names:
        sys.exit(f"{args.dir} has no latest.json; run `sign` first")
    with open(os.path.join(args.dir, "latest.json"), encoding="utf-8") as f:
        latest = json.load(f)
    wanted = {"latest.json"} | {p["file"] for p in latest.get("platforms", {}).values()}
    missing = wanted - set(names)
    if missing:
        sys.exit(f"missing in {args.dir}: {', '.join(sorted(missing))}")

    boundary = uuid.uuid4().hex
    parts = []
    for name in sorted(wanted):
        with open(os.path.join(args.dir, name), "rb") as f:
            data = f.read()
        head = (f'--{boundary}\r\nContent-Disposition: form-data; name="file"; '
                f'filename="{name}"\r\nContent-Type: application/octet-stream\r\n\r\n')
        parts.append(head.encode() + data + b"\r\n")
    body = b"".join(parts) + f"--{boundary}--\r\n".encode()
    url = args.server.rstrip("/") + "/admin/api/releases?live=" + ("true" if args.live else "false")
    req = urllib.request.Request(url, data=body, method="POST", headers={
        "Authorization": f"Bearer {args.token}",
        "Content-Type": f"multipart/form-data; boundary={boundary}",
    })
    try:
        with urllib.request.urlopen(req, timeout=600) as r:
            res = json.load(r)
    except urllib.error.HTTPError as e:
        try:
            msg = json.load(e).get("error", "")
        except Exception:
            msg = ""
        sys.exit(f"Upload failed: HTTP {e.code} {msg}")
    print(f"Uploaded release {res['release']['version']}"
          + (" and made it live." if args.live else " (not live yet; make it live in the dashboard)."))


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    k = sub.add_parser("keygen", help="make a new signing key pair")
    k.add_argument("--key", required=True, help="where to write the private key (PEM)")
    k.set_defaults(func=cmd_keygen)

    pk = sub.add_parser("pubkey", help="print the public key of a private key")
    pk.add_argument("--key", required=True)
    pk.set_defaults(func=cmd_pubkey)

    s = sub.add_parser("sign", help="sign an installer and add it to latest.json")
    s.add_argument("--key", required=True, help="private key (PEM)")
    s.add_argument("--file", required=True, help="installer to publish")
    s.add_argument("--out", default="release", help="release folder to upload (default: release)")
    s.add_argument("--version", help="release version (default: Cargo.toml version)")
    s.add_argument("--platform", help="e.g. windows-x86_64, macos-x86_64, linux-x86_64")
    s.add_argument("--notes", help="short release notes shown to users")
    s.set_defaults(func=cmd_sign)

    u = sub.add_parser("upload", help="publish a signed release folder to a Tether update server")
    u.add_argument("--server", required=True, help="update server URL, e.g. https://updates.example.com")
    u.add_argument("--token", default=os.environ.get("TETHER_UPLOAD_TOKEN"),
                   help="upload token from the dashboard (or env TETHER_UPLOAD_TOKEN)")
    u.add_argument("--dir", default="release", help="release folder (default: release)")
    u.add_argument("--live", action="store_true", help="make it the live release right away")
    u.set_defaults(func=cmd_upload)

    args = p.parse_args()
    if args.cmd == "upload" and not args.token:
        sys.exit("--token (or TETHER_UPLOAD_TOKEN) is required")
    args.func(args)


if __name__ == "__main__":
    main()
