//! The `sftp://` connection type. Port of `SFTPProvider` in `remote/providers.py`.

use std::sync::Arc;

use oc_provider::{
    ConnectContext, ConnectError, Field, FieldKind, FileSystemProvider, Params, Value, join_server_url, param_int,
    param_str, quote, split_server_url,
};
use oc_vfs::FileSystem;

use crate::connect::{APP_NAME, ConnectOptions, DEFAULT_PORT, HostKeyQuestion, connect, expand_user};
use crate::fs::SftpFileSystem;
use crate::paths;

#[derive(Clone, Copy, Debug, Default)]
pub struct SftpProvider;

/// The "trust this host key?" question as the 1.x version words it.
pub fn host_key_message(q: &HostKeyQuestion) -> String {
    let shown = if q.port == DEFAULT_PORT { q.host.clone() } else { format!("{}:{}", q.host, q.port) };
    format!(
        "{APP_NAME} hasn't connected to {shown} before. Its identity can't be confirmed yet.\n\n\
         {} key fingerprint:\n  {}\n\n\
         If you know the server's fingerprint, check that it matches. Trusting it remembers this key; \
         {APP_NAME} will refuse to connect if it ever changes.",
        q.key_type, q.fingerprint
    )
}

impl FileSystemProvider for SftpProvider {
    fn scheme(&self) -> &'static str {
        "sftp"
    }

    fn title(&self) -> &'static str {
        "SFTP (SSH)"
    }

    fn fields(&self) -> Vec<Field> {
        vec![
            Field { required: true, placeholder: "server name or address", ..Field::text("host", "Server") },
            Field { kind: FieldKind::Int, placeholder: "default 22", ..Field::text("port", "Port") },
            Field::text("username", "User"),
            Field {
                kind: FieldKind::Path,
                placeholder: "optional, e.g. ~/.ssh/id_ed25519",
                ..Field::text("key_file", "Key file")
            },
            Field { placeholder: "/absolute or relative to home", ..Field::text("path", "Start folder") },
        ]
    }

    fn secret_hint(&self) -> &'static str {
        "empty: use SSH agent/keys, or ask"
    }

    /// `sftp://[user@]host[:port][/abs/path | /~/path/in/home]`
    fn parse_url(&self, url: &str) -> Result<Params, String> {
        let (mut params, segments) = split_server_url(url, self.scheme())?;
        if segments.first().is_some_and(|s| s == "~") {
            params.insert("path".into(), Value::String(segments[1..].join("/"))); // relative to home
        } else if !segments.is_empty() {
            params.insert("path".into(), Value::String(format!("/{}", segments.join("/"))));
        }
        Ok(params)
    }

    fn format_url(&self, params: &Params) -> String {
        let path = param_str(params, "path");
        let suffix = if path.starts_with('/') {
            quote(&path, true)
        } else {
            let rel = path.trim_matches('/');
            if rel.is_empty() { String::new() } else { format!("/~/{}", quote(rel, true)) }
        };
        join_server_url(self.scheme(), params, Some(i64::from(DEFAULT_PORT)), &suffix)
    }

    fn connect(
        &self,
        params: &Params,
        secret: Option<&str>,
        ctx: &dyn ConnectContext,
    ) -> Result<Arc<dyn FileSystem>, ConnectError> {
        let port_text = param_str(params, "port");
        let port = match param_int(params, "port") {
            _ if port_text.trim().is_empty() => DEFAULT_PORT,
            Some(0) => DEFAULT_PORT,
            Some(p) => u16::try_from(p).map_err(|_| ConnectError::Connection(format!("Invalid port: {p}")))?,
            None => return Err(ConnectError::Connection(format!("Invalid port: {port_text}"))),
        };
        let mut opts = ConnectOptions::new(param_str(params, "host"), ctx.data_dir().join("known_hosts"));
        opts.port = port;
        opts.username = Some(param_str(params, "username")).filter(|u| !u.is_empty());
        opts.password = secret.filter(|s| !s.is_empty()).map(str::to_string);
        let key_file = param_str(params, "key_file");
        opts.key_file = (!key_file.trim().is_empty()).then(|| expand_user(key_file.trim()));
        let confirm = |q: &HostKeyQuestion| ctx.confirm("Unknown server", &host_key_message(q), "Trust and connect");
        Ok(connect(&opts, &confirm)?)
    }

    /// The home folder, or the start folder (absolute or relative to home).
    fn start_path(&self, params: &Params, fs: &Arc<dyn FileSystem>) -> String {
        let home = SftpFileSystem::find(fs).map_or_else(|| "/".to_string(), |s| s.home().to_string());
        let path = param_str(params, "path");
        if path.is_empty() {
            return home;
        }
        paths::normpath(&if path.starts_with('/') { path } else { paths::join(&home, &[&path]) })
    }

    fn is_connected(&self, fs: &Arc<dyn FileSystem>) -> bool {
        SftpFileSystem::find(fs).is_none_or(|s| s.is_connected())
    }
}
