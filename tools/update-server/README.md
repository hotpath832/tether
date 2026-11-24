# Tether update server

One Windows executable that serves Tether updates, with an admin dashboard.

- `GET /latest.json` and `GET /<installer>`: what Tether clients use.
- `/admin`: dashboard to upload releases, choose the live one, roll back, and see statistics.

The server refuses any release that isn't signed with Tether's release key, the same key the
clients check. Someone who steals the admin password still can't ship their own build.

## Build

```powershell
cd tools\update-server
cargo build --release
```

The result is `target\release\tether-update-server.exe`. It's self-contained, so nothing
else needs to be installed on the server.

## Install on the Windows VPS

1. Copy `tether-update-server.exe` and `install-service.ps1` to the VPS, in the same folder.
2. In an **Administrator** PowerShell, run:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install-service.ps1 -Port 8080
   ```

   The script does the following:
   - installs the server to `C:\TetherUpdateServer`
   - registers a startup task that runs it as SYSTEM and restarts it if it stops
   - opens the port in Windows Firewall
   - starts the server and prints the generated first admin password
3. If your VPS provider has its own firewall or security group, allow the port there too.
4. Open `http://<vps-address>:8080/admin`, sign in, and change the password under **Settings**.
5. In each Tether, go to Settings → General → Update. Set **Update server** to
   `http://<vps-address>:8080` and turn on **Auto update**.

Use the same script to upgrade the server later. Your data and settings are kept.

### HTTPS (recommended)

Updates stay safe over plain HTTP because they're signed. The admin password, however, travels
unencrypted. To add HTTPS:

1. Point a domain at the VPS.
2. Get a certificate in PEM format, for example with [win-acme](https://www.win-acme.com/) using
   its "PEM encoded files" store.
3. Edit `C:\TetherUpdateServer\data\server.json`:
   ```json
   { "listen": "0.0.0.0:443",
     "tls_cert": "C:\\certs\\updates.example.com-chain.pem",
     "tls_key":  "C:\\certs\\updates.example.com-key.pem" }
   ```
4. Restart the task by running `Restart-ScheduledTask TetherUpdateServer`, or reboot.
5. Open port 443 in the firewall.

## Publishing a release

On the build PC, which holds the signing key at `C:\tether-keys\update_signing_key.pem`:

1. Raise `version` in `Cargo.toml`, `libs/portable/Cargo.toml` and `flutter/pubspec.yaml`, and
   for the `rustdesk` / `rustdesk-portable-packer` entries in `Cargo.lock`. Then build.
2. Sign the installer:
   ```powershell
   python res\tether_release.py sign --key C:\tether-keys\update_signing_key.pem --file tether-1.5.1-install.exe --out release
   ```
3. Upload it in one of two ways:
   - Dashboard: **Upload** tab. Drop in `release\latest.json` and the installer, and keep
     "Make live" ticked.
   - Command line, using a token from the dashboard's **Settings** tab:
     ```powershell
     python res\tether_release.py upload --server http://<vps-address>:8080 --token tus_... --dir release --live
     ```

Clients check 30 seconds after Tether starts and then once a day. With Auto update on, they
install in the background when no session is open. Otherwise they show "New version available".

## Rollback

On the **Releases** tab:

- **Make live** on an older release: clients still on old versions now get that one instead.
- **Unpublish**: no update is offered at all.

Installs that already took the bad version keep it, because clients never downgrade. To fix
them, publish a newer version.

## Command line

```text
tether-update-server [--data <dir>]                    run (data defaults to .\data)
tether-update-server [--data <dir>] set-password <pw>  reset the admin password
tether-update-server [--data <dir>] new-token          print a new upload token
```

## Data folder

| File | Contents |
|---|---|
| `server.json` | listen address and TLS paths |
| `auth.json` | password hash and token hash |
| `catalog.json` | releases and which one is live |
| `stats.json` | daily check and download counts, kept for 400 days; no IP addresses |
| `releases\<version>\` | the installers |

Back up this folder.
