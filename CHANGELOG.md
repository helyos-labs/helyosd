# Changelog

## 0.3.3

### Fixed

- **Atomic proxy-config writes.** The Traefik, nginx, and Caddy backends wrote their
  config with a truncate-then-write, so the proxy's file watcher could read a partial file
  mid-write (e.g. Traefik's *"routers cannot be a standalone element"*) and drop the route.
  Configs are now written to a temp file and atomically renamed into place.
- **Public deployments are not host-published on a random port.** The Docker runtime
  adapter published every declared port even when no host port was requested, so a public
  deployment (reached through the proxy) ended up bound on a random host port. Ports
  without a host port are now exposed on the container network only.
- The configured `--acme-email` is now passed through to the orchestrator, so automatic-TLS
  routes can actually be created (see helyos-core 0.1.8).

### Changed

- Depends on helyos-core 0.1.8.
