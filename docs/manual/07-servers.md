# Servers

A panel can show a folder on a server. You can browse it, preview files,
copy and move files in both directions, create folders, delete files, and
browse archives stored on the server.

Supported connection types:

| Type | Use for |
| --- | --- |
| SFTP | Servers you reach with SSH |
| SMB | Windows shares, Samba servers and most NAS devices |
| WebDAV | Nextcloud, ownCloud, NAS devices, web servers with WebDAV |
| S3 | Amazon S3 and S3-compatible storage such as MinIO, Cloudflare R2, Backblaze B2, Wasabi and DigitalOcean Spaces |

## Connecting with an address

1. Press **⌘G** / **Ctrl+G**.
2. Type the server address and press **Enter**.

| Type | Address |
| --- | --- |
| SFTP | `sftp://user@host/absolute/path`, or `sftp://user@host/~/folder` for a folder in your home directory |
| SMB | `smb://user@server/share/folder` |
| WebDAV over HTTPS | `webdavs://user@host/server/path` |
| WebDAV over HTTP | `webdav://user@host/server/path` |
| S3 over HTTPS | `s3://access-key@server/bucket/folder`, or `s3://bucket/folder` for Amazon S3 |
| S3 over HTTP | `s3+http://access-key@server/bucket/folder` |

Add a port after the host name if the server doesn't use the standard port,
for example `sftp://anna@example.com:2222/srv`. For S3 servers that need a
region, add `?region=` and the region name.

If the server needs a password, Orchard Commander asks for it. Select
**Remember in the system keychain** to save it.

## Saved connections

Press **⌘K** / **Ctrl+K** to open **Connect to server**. It lists your saved
connections.

| Button | Action |
| --- | --- |
| Connect | Open the selected connection in the active panel |
| New… | Create a connection |
| Edit… | Change the selected connection |
| Delete | Remove the selected connection |

Saved connections also appear in the drive list (**⌥F1** / **Alt+F1** and
**⌥F2** / **Alt+F2**), marked ⇄.

### Creating a connection

1. Press **⌘K** / **Ctrl+K** and click **New…**.
2. Select the **Type**.
3. Optionally enter a **Name**, which is shown in the connection list.
4. Fill in the fields for the connection type (see below).
5. Enter the password or secret key, if needed. Select
   **Remember in the system keychain** to save it.
6. Click **Save**.

### Connection fields

**SFTP (SSH)**

| Field | Description |
| --- | --- |
| Server | Host name or IP address |
| Port | Leave empty for port 22 |
| User | Your user name on the server |
| Key file | Optional private key file, for example `~/.ssh/id_ed25519`. Enter its passphrase in the password field. |
| Start folder | The folder to open, absolute or relative to your home directory |

Without a password or key file, Orchard Commander uses your SSH agent and
your default SSH keys.

**SMB (Windows / Samba share)**

| Field | Description |
| --- | --- |
| Server | Host name or IP address |
| Port | Leave empty for port 445 |
| Share | The name of the share, for example `media` |
| User | Your user name |
| Start folder | A folder inside the share |

You must enter the share name; Orchard Commander can't list the shares of
a server.

**WebDAV (HTTPS) and WebDAV (HTTP, unencrypted)**

| Field | Description |
| --- | --- |
| Server | Host name, for example `cloud.example.com` |
| Port | Leave empty for 443 (HTTPS) or 80 (HTTP) |
| Server path | The WebDAV path on the server. For Nextcloud: `/remote.php/dav/files/USER`, where USER is your user name |
| User | Your user name |
| Start folder | A folder below the server path |

Tip: For Nextcloud, create an app password in your Nextcloud security
settings and use it instead of your account password.

**S3 (Amazon S3 and compatible) and S3 (HTTP, unencrypted)**

| Field | Description |
| --- | --- |
| Server | Leave empty for Amazon S3, or enter the server, for example `minio.local` |
| Port | Leave empty for 443 (HTTPS) or 80 (HTTP) |
| Region | Leave empty to detect it automatically |
| Access key ID | Leave empty to read public buckets only |
| Start folder | A bucket and folder, for example `photos/2026`. Leave empty to list your buckets. |

Enter the secret access key in the **Secret key** field.

## Server security

**SFTP host keys.** The first time you connect to an SFTP server, Orchard
Commander shows the server's key fingerprint and asks whether to trust it.
Compare the fingerprint with the one your server administrator gave you. A
trusted key is remembered. If a server's key changes later, the connection
is refused.

**Certificates.** WebDAV and S3 over HTTPS check the server's certificate
against your system's trusted certificates. If a server uses a certificate
your system doesn't trust, such as a self-signed certificate on a NAS, its
fingerprint is shown and you can trust it. If that certificate changes
later, the connection is refused.

**Passwords** are saved only if you select
**Remember in the system keychain**. They are stored in the macOS Keychain,
the Windows Credential Manager or the Linux Secret Service, not in Orchard
Commander's settings files.

## Working on a server

A server panel works like a local panel, with these differences:

- **F8 deletes permanently.** Servers have no trash, and the confirmation
  says so.
- **Opening a file** downloads it to a temporary folder first.
- Moving files within the same server is done on the server, without
  downloading them.
- **S3:** The top level lists your buckets. **F7** creates a bucket there,
  and **F8** deletes a bucket with all its contents. Below the top level,
  folders work as in the S3 web console. S3 doesn't keep modification times
  you set; files show the time they were uploaded.

## Disconnecting

Press **⇧⌘K** / **Ctrl+Shift+K** to close the connection of the server shown
in the active panel. Panels showing that server switch to your home folder.

## Servers on your local network (macOS)

The first time Orchard Commander connects to a server on your local network,
macOS asks whether to allow it to find and connect to devices on your local
network. Click **Allow**. If you clicked **Don't Allow**, see
[Troubleshooting](14-troubleshooting.md#a-server-on-my-network-cant-be-reached-macos).
