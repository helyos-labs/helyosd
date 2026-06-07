<div align="center">

<img alt="Helyos" src="assets/helyos-logo.png" width="200">

# helyosd

**Helyos daemon -- container orchestration engine**

[![CI](https://github.com/helyos-labs/helyosd/actions/workflows/ci.yml/badge.svg)](https://github.com/helyos-labs/helyosd/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)

helyosd is the server component of [Helyos](https://github.com/helyos-labs/helyos):
80% of the Kubernetes use-cases at 20% of the complexity. It provides concrete
adapter implementations for every [helyos-core](https://github.com/helyos-labs/helyos-core)
port trait, exposes an HTTPS REST API on port 6443, and supports single-node,
master, and worker clustering modes over gRPC.

[Quick Start](#quick-start) · [Security / API access](#security--api-access) · [REST API](#rest-api) · [Clustering](#clustering)

</div>

---

## Features

- **Container runtimes** -- Docker (via bollard) and containerd (via ctr CLI) with automatic detection
- **Secure by default** -- HTTPS with an auto-generated self-signed cert, an auto-generated API bearer token, and a hard guardrail against exposing a non-loopback address in the clear
- **kubectl-style remote control** -- mint per-user API tokens, pin the daemon CA from the `helyos` CLI, and manage clusters from anywhere
- **Persistent state** -- SQLite-backed store for projects, deployments, pods, nodes, and cluster config
- **Encrypted secrets** -- AES-256-GCM encryption at rest with an auto-generated master key
- **REST API** -- axum-based API on port 6443 with full CRUD for all resources, SSE event stream, and Prometheus metrics
- **Multi-node clustering** -- master/worker topology over gRPC (TLS, server-authenticated via the master's self-signed CA) with join tokens and heartbeat-driven rescheduling
- **Reverse proxy integration** -- pluggable backends: traefik (default), nginx, caddy
- **Automatic TLS for routes** -- ACME certificate provisioning and daily renewal
- **Overlay networking** -- WireGuard-based mesh with CNI plugin support and per-project subnet allocation
- **Embedded DNS** -- Hickory DNS server for service discovery (`<deployment>.<project>.internal`)
- **Health checking** -- background probe runner with orchestrator-driven restart
- **Container event watcher** -- real-time container lifecycle events fed back to the orchestrator
- **237 tests** (unit + integration)

## Quick Start

### Prerequisites

- Rust 1.88+ (edition 2024)
- Docker or containerd running on the host

### Build and run

```bash
# Build
cargo build --release

# Start in single-node mode (default). On first run helyosd:
#   1. generates an API token and logs it once
#   2. serves the API (default --tls auto + loopback bind = plain HTTP;
#      a non-loopback bind or --tls on enables HTTPS and writes a self-signed cert)
#   3. writes a ready-to-use local CLI context to ~/.helyos/config.toml
./target/release/helyosd

# Start with custom options
./target/release/helyosd \
    --host 0.0.0.0 \
    --port 6443 \
    --advertise-addr cluster.example.com \
    --data-dir /var/lib/helyos \
    --runtime auto
```

Because the daemon writes a local CLI context on first start, local use is
zero-config: run `helyosd`, then the `helyos` CLI just works against the daemon.

### Deploy a service

```bash
# Using the CLI (uses the zero-config local context)
helyos deploy examples/app.yaml

# Or directly via the API (HTTPS + bearer token)
curl -sk https://localhost:6443/api/v1/deploy \
    -H "Authorization: Bearer $HELYOS_API_TOKEN" \
    -H 'Content-Type: application/json' \
    -d @spec.json
```

## Security / API access

helyosd is secure by default.

**HTTPS.** TLS mode is `auto`: the daemon serves HTTPS unless it is bound to a
loopback address, in which case it stays plain HTTP for convenience. On first
start it generates a self-signed certificate (with SANs derived from the bind
host, `--advertise-addr`, any `--tls-san` values, and the system hostname) and
stores it under the data directory. Force the behaviour with `--tls on|off`, or
bring your own certificate with `--tls-cert` + `--tls-key`.

A non-loopback bind over plain HTTP is refused unless you pass **both**
`--tls off` and `--insecure-http`, and even then only if an API token is
configured.

**API token.** All `/api/v1/*` routes except `version` and `ca` require a
`Bearer` token. On first run the daemon generates one and logs it once
(`HELYOS_API_TOKEN=...`); save it. You can instead supply your own via
`--api-token` or the `HELYOS_API_TOKEN` environment variable. Tokens are stored
as Argon2id hashes — the plaintext is never persisted. Mint additional named
tokens at runtime via `POST /api/v1/tokens` (the secret is shown once).

**CLI login & CA pinning.** From the `helyos` CLI:

```bash
# Pin the daemon CA and store a named connection context
helyos login https://cluster.example.com:6443 \
    --token "$HELYOS_API_TOKEN" \
    --ca-fingerprint <sha256-from /api/v1/ca>

helyos whoami     # identity of the current token
helyos auth token ls   # list / mint / revoke server-side API tokens
helyos context         # manage connection contexts
```

The daemon prints the matching `helyos login` hint (including how to read the CA
fingerprint) whenever HTTPS is enabled.

## Deployment Specs

helyosd accepts YAML deployment specs. Two examples are included:

**examples/app.yaml** -- a multi-replica API service:

```yaml
project: ecommerce

deployment:
  name: api

replicas: 3
image: ghcr.io/company/api:latest

ports:
  - 3000

network:
  public: true
  domain: api.example.com
  https: true

env:
  DATABASE_URL: "postgres://<db-user>:<db-password>@<db-host>:5432/ecommerce"
  REDIS_URL: "redis://<redis-host>:6379"

healthcheck:
  path: /health
  interval: 10s
```

**examples/nginx.yaml** -- a simple web server:

```yaml
project: demo

deployment:
  name: nginx

replicas: 1
image: nginx:alpine

ports:
  - 8080

network:
  public: true

healthcheck:
  path: /
  interval: 10s
```

## CLI Flags

```
helyosd [OPTIONS]

Options:
    --host <HOST>               Listen address [default: 127.0.0.1]
    --port <PORT>               HTTP(S) API port [default: 6443]
    --data-dir <DIR>            Data directory [default: ~/.helyos/data]
    --mode <MODE>               Node mode: single, master, worker [default: single]
    --runtime <RUNTIME>         Container runtime: docker, containerd, auto [default: auto]

  Security / API access:
    --tls <MODE>                TLS mode: auto, on, off [default: auto]
                                (auto = TLS unless bound to loopback)
    --tls-cert <PATH>           PEM certificate for the API (BYO; overrides self-signed)
    --tls-key <PATH>            PEM private key for --tls-cert
    --tls-san <SAN>             Extra SAN (DNS name or IP) for the self-signed cert (repeatable)
    --advertise-addr <ADDR>     Public address clients dial (baked into cert SAN + login hint)
    --insecure-http             Allow a non-loopback bind over plain HTTP (with --tls off)
    --api-token <TOKEN>         API bearer token (or set HELYOS_API_TOKEN)

  Clustering:
    --join <ADDR>               Master address (worker mode)
    --token <TOKEN>             Join token (worker mode)
    --grpc-port <PORT>          gRPC port for cluster communication [default: 6444]

  DNS:
    --dns-mode <MODE>           DNS mode: noop, embedded [default: noop]
    --dns-listen <ADDR>         Embedded DNS listen address [default: 127.0.0.1:15353]
    --dns-upstream <ADDR>       Upstream DNS server [default: 8.8.8.8:53]
    --master-ip <IP>            Node IP for container DNS config (embedded mode)

  Proxy / networking:
    --proxy-backend <BACKEND>   Proxy: traefik, nginx, caddy [default: traefik]
    --proxy-config-dir <DIR>    Proxy config directory [default: <data-dir>/proxy]
    --acme-email <EMAIL>        ACME email for automatic route TLS
    --cluster-cidr <CIDR>       Overlay network CIDR [default: 172.20.0.0/16]
    --wg-port <PORT>            WireGuard listen port [default: 51820]
    --overlay                   Enable WireGuard overlay network
```

## Architecture

```
                    +-------------------+
                    |    helyos (CLI)     |
                    +--------+----------+
                             |  HTTPS + Bearer token
                             v
+-----------------------------------------------------------+
|  helyosd                                                    |
|                                                           |
|  +------------------+    +-----------------------------+  |
|  |   REST API       |    |   gRPC Cluster Server       |  |
|  |   (axum :6443)   |    |   (tonic :6444, TLS)        |  |
|  +--------+---------+    +-------------+---------------+  |
|           |                            |                  |
|           v                            v                  |
|  +--------------------------------------------------+    |
|  |              Orchestrator (actor loop)            |    |
|  |   mpsc/oneshot channels -- 24 command variants    |    |
|  +------+-------+-------+-------+-------+-----------+    |
|         |       |       |       |       |                 |
|         v       v       v       v       v                 |
|  +---------+ +-----+ +------+ +-----+ +-------+          |
|  |Container| |State| |Secret| | DNS | | Proxy  |         |
|  |Runtime  | |Store| |Store | |     | |Backend |         |
|  +---------+ +-----+ +------+ +-----+ +-------+          |
|   Docker/    SQLite   AES-GCM  Hickory  traefik/         |
|   containerd          SQLite   DNS      nginx/caddy      |
+-----------------------------------------------------------+
```

### Adapter implementations

| Port Trait | Adapter | Details |
|---|---|---|
| `ContainerRuntime` | `DockerRuntime` | bollard crate, Docker Engine API |
| `ContainerRuntime` | `ContainerdRuntime` | ctr CLI wrapper |
| `StateStore` | `SqliteStore` | sqlx with migrations |
| `SecretStore` | `EncryptedSqliteSecretStore` | AES-256-GCM, rusqlite |
| `ClusterTransport` | gRPC client/server | tonic + prost, protobuf, TLS (server-authenticated) |
| `ClusterTransport` | `LocalTransport` | single-node passthrough |
| `DnsProvider` | `HickoryDnsProvider` | embedded DNS server |
| `DnsProvider` | `NoopDnsProvider` | Docker DNS fallback |
| `ProxyBackend` | `TraefikBackend` | generates dynamic YAML config (default) |
| `ProxyBackend` | `NginxBackend` | generates nginx.conf |
| `ProxyBackend` | `CaddyBackend` | generates Caddyfile + API reload |

## REST API

All resource endpoints are under `/api/v1/`. The API listens on port 6443 (HTTPS
by default). **Auth** marks which routes require a `Bearer` token; the rest are
public so clients can probe reachability and pin the CA before logging in.

| Method | Endpoint | Auth | Description |
|---|---|:--:|---|
| `GET` | `/health` | — | Health check |
| `GET` | `/metrics` | — | Prometheus metrics |
| `GET` | `/api/v1/version` | — | Daemon version / TLS reachability probe |
| `GET` | `/api/v1/ca` | — | Self-signed CA PEM + SHA-256 fingerprint (for pinning) |
| `POST` | `/api/v1/tokens` | Bearer | Mint a named API token (secret shown once) |
| `GET` | `/api/v1/tokens` | Bearer | List API tokens (never the secret) |
| `DELETE` | `/api/v1/tokens/{name}` | Bearer | Revoke an API token |
| `GET` | `/api/v1/whoami` | Bearer | Identity of the calling token |
| `POST` | `/api/v1/deploy` | Bearer | Deploy from spec |
| `GET` | `/api/v1/deployments` | Bearer | List deployments |
| `POST` | `/api/v1/projects` | Bearer | Create project |
| `GET` | `/api/v1/projects` | Bearer | List projects |
| `POST` | `/api/v1/projects/{name}/suspend` | Bearer | Suspend project |
| `POST` | `/api/v1/projects/{name}/resume` | Bearer | Resume project |
| `DELETE` | `/api/v1/projects/{name}` | Bearer | Delete project |
| `POST` | `/api/v1/projects/{p}/deployments/{d}/scale` | Bearer | Scale deployment |
| `POST` | `/api/v1/projects/{p}/deployments/{d}/stop` | Bearer | Stop deployment |
| `DELETE` | `/api/v1/projects/{p}/deployments/{d}` | Bearer | Remove deployment |
| `GET` | `/api/v1/projects/{p}/deployments/{d}/logs` | Bearer | Stream logs |
| `GET` | `/api/v1/pods` | Bearer | List pods |
| `GET` | `/api/v1/projects/{p}/secrets` | Bearer | List secrets |
| `POST` | `/api/v1/projects/{p}/secrets/{n}` | Bearer | Set secret |
| `DELETE` | `/api/v1/projects/{p}/secrets/{n}` | Bearer | Delete secret |
| `GET` | `/api/v1/nodes` | Bearer | List nodes |
| `GET` | `/api/v1/nodes/stats` | Bearer | Node resource stats |
| `POST` | `/api/v1/nodes/{name}/drain` | Bearer | Drain node |
| `DELETE` | `/api/v1/nodes/{name}` | Bearer | Remove node |
| `GET` | `/api/v1/events` | Bearer | Cluster event stream (SSE) |
| `GET` | `/api/v1/routes` | Bearer | List routes |
| `POST` | `/api/v1/routes` | Bearer | Add route |
| `DELETE` | `/api/v1/routes/{domain}` | Bearer | Remove route |
| `POST` | `/api/v1/certs/import` | Bearer | Import TLS certificate |
| `POST` | `/api/v1/cluster/init` | Bearer | Initialize cluster |
| `GET` | `/api/v1/cluster/token` | Bearer | Show join token |
| `POST` | `/api/v1/cluster/token/rotate` | Bearer | Rotate join token |
| `GET` | `/api/v1/cluster/scheduler` | Bearer | Get scheduler config |
| `POST` | `/api/v1/cluster/scheduler` | Bearer | Set scheduler config |
| `GET` | `/api/v1/cluster/config/proxy` | Bearer | Get proxy config |
| `POST` | `/api/v1/cluster/config/proxy` | Bearer | Set proxy config |

## Clustering

helyosd supports a master/worker topology for multi-node deployments. Cluster
gRPC traffic runs over TLS: the master serves a self-signed certificate (signed
by a CA it generates), and workers verify it against that CA. Workers
authenticate to the master with a join token, not a client certificate.

```bash
# Start the master
helyosd --mode master --dns-mode embedded --master-ip 10.0.1.1 --overlay

# The master prints a join command:
#   helyosd --mode worker --join 10.0.1.1:6444 --token <TOKEN>

# On worker nodes
helyosd --mode worker \
    --join 10.0.1.1:6444 \
    --token <TOKEN> \
    --overlay
```

Workers register via gRPC, send periodic heartbeats, and receive pod assignments
from the master. The heartbeat monitor detects dead nodes, marks their pods as
failed, and triggers the orchestrator to reschedule the affected deployments
onto healthy nodes.

## Development

```bash
# Build
cargo build

# Run all tests (237 total)
cargo test

# Run only unit tests
cargo test --lib

# Run integration tests (some require Docker)
cargo test --test api_integration
cargo test --test sqlite_integration
cargo test --test runtime_integration
cargo test --test e2e
```

## Related Repositories

| Repository | Description |
|---|---|
| [helyos](https://github.com/helyos-labs/helyos) | Meta repository: overview and documentation |
| [helyos-core](https://github.com/helyos-labs/helyos-core) | Core domain types, traits, and orchestrator |
| [helyos-cli](https://github.com/helyos-labs/helyos-cli) | CLI tool for deploying and managing containers |

## License

Apache-2.0 -- see [LICENSE](LICENSE) for details.
