mod caddy;
mod nginx;
mod traefik;

pub use caddy::CaddyBackend;
pub use nginx::NginxBackend;
pub use traefik::TraefikBackend;

use std::path::Path;

/// Write `contents` to `path` atomically: write a sibling temp file, then rename it over
/// the destination. Proxies (Traefik, nginx, Caddy) watch these files; a plain
/// truncate-then-write can be observed mid-write as a partial/empty file, which the proxy
/// then rejects (e.g. Traefik's "routers cannot be a standalone element"). A rename is
/// atomic on the same filesystem, so watchers only ever see the complete old or new file.
pub(crate) async fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = match path.file_name() {
        Some(name) => {
            let mut t = name.to_os_string();
            t.push(".tmp");
            path.with_file_name(t)
        }
        None => path.with_extension("tmp"),
    };
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
