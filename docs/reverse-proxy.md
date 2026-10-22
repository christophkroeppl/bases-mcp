# Running behind a reverse proxy

TLS in transit, terminated in front of the listener.

The server speaks **plaintext HTTP** and has no TLS of its own: `MCP_HTTP_HOST`,
`MCP_HTTP_PORT` and `MCP_TRANSPORT` are the entire HTTP surface
(`src/transport.rs`), and `src/config.rs` selects a vault and nothing else. There
is no certificate variable, no key variable and no `*_TLS` switch to set, because
there is no TLS code to switch on. TLS is terminated by whatever sits in front of
the listener, and the listener never sees a TLS byte.

That leaves four things to get right: **where it dials**, **what `Host` it
sends**, **that it does not buffer the response**, and **that it does not rewrite
the request**. The first two are fixed by the server and by the compose network;
the last two are proxy settings, and the cost of getting them wrong is a stream
that connects and then produces nothing.

| | Value | Where it comes from |
|---|---|---|
| Upstream address | `bases-mcp:8765` (same compose network) — or `127.0.0.1:8765` when the proxy runs on the host | `MCP_HTTP_PORT`, default `8765` (`src/transport.rs`) |
| Upstream path | `/mcp`, **unchanged** | `HTTP_PATH` — `src/transport.rs`, mounted by `nest_service` in `src/main.rs` |
| `Host` the server accepts | any, if the container binds `0.0.0.0`; otherwise `localhost` / the bound address | `allowed_hosts_for` — `src/transport.rs` |
| TLS termination | at the proxy | there is nothing to terminate in the server |

## What the server accepts as `Host`

`allowed_hosts` is derived from the bind address and **is not configurable**. It
is also not optional: rmcp rejects a `Host` it does not recognise with a bare
`403 Forbidden: Host header is not allowed`, before the request reaches a tool.

| `MCP_HTTP_HOST` | `Host` that is served |
|---|---|
| `127.0.0.1` (default) | `localhost`, `127.0.0.1` |
| a specific address | that address, plus `localhost` |
| `0.0.0.0` / `::` | **any** — every interface, so no address is knowable |

The port is not part of the match. Every entry `allowed_hosts_for` builds has no
port in it, and rmcp treats a portless allowed entry as matching any port
(`host_is_allowed` in `rmcp-3.5.0`'s `streamable_http_server/tower.rs`). So
`Host: 127.0.0.1:8765` and `Host: 127.0.0.1` are both accepted on a loopback
bind. A proxy that forwards `Host: mcp.example.com` is **refused** on that bind
and **accepted** only on the `0.0.0.0` bind.

Inside a container the bind **must** be `0.0.0.0` — the `127.0.0.1` default
binds the loopback *of the container* and nothing outside it can reach the
listener. That widens which `Host` values are accepted, and the server says so
on stderr at startup; it is recorded as D9 in `docs/divergences.md`. This is why
every example below sets `MCP_HTTP_HOST: "0.0.0.0"` and can leave `Host` alone.

## The three examples

Each is a complete compose project — the proxy, the server behind it, and a
network between them — plus the one config file the proxy needs. Nothing is
published to the host by `bases-mcp`: the only published ports belong to the
proxy, and the listener is reachable only from the proxy. That is the boundary
`docs/tooling.md` names — where the port is *published*, not where the server
binds — and a reverse proxy is what lets the listener stay unpublished.

The vault mount needs the uid mapping `docs/tooling.md` describes (`--userns=keep-id`
under rootless podman). A container that can read the vault and not write it is
that section's failure, and it looks like a working server.

If the proxy is **not** in this compose project, give the service a loopback
publication (`- "127.0.0.1:8765:8765"`) and dial `127.0.0.1:8765` instead of
`bases-mcp:8765`. Never publish it on `0.0.0.0`: that puts an unauthenticated
listener on the network, which is the thing the proxy exists to prevent.

### Traefik

```yaml
services:
  traefik:
    image: traefik:v3.7
    command:
      - --entrypoints.web.address=:80
      - --entrypoints.websecure.address=:443
      - --providers.docker=true
      - --providers.docker.exposedbydefault=false
      - --certificatesresolvers.le.acme.email=you@example.com
      - --certificatesresolvers.le.acme.storage=/certs/acme.json
      - --certificatesresolvers.le.acme.tlschallenge=true
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - traefik-certs:/certs
    networks: [mcp]

  bases-mcp:
    image: localhost/bases-mcp:latest
    command: ["--transport", "http"]
    environment:
      BASES_MCP_VAULT: /vault
      # Required in a container: 127.0.0.1 would be the container's own
      # loopback. This is also what makes any Host acceptable, so Traefik can
      # forward the client's Host unchanged.
      MCP_HTTP_HOST: "0.0.0.0"
      MCP_HTTP_PORT: "8765"
    volumes:
      - /path/to/vault:/vault:rw
    expose: ["8765"]
    networks: [mcp]
    labels:
      - traefik.enable=true
      - traefik.http.routers.mcp.rule=Host(`mcp.example.com`)
      - traefik.http.routers.mcp.entrypoints=websecure
      - traefik.http.routers.mcp.tls.certresolver=le
      - traefik.http.services.mcp.loadbalancer.server.port=8765

networks:
  mcp:

volumes:
  traefik-certs:
```

Traefik dials `bases-mcp:8765` — the service name from the compose network, the
port from `MCP_HTTP_PORT` — and TLS terminates at the `websecure` entrypoint
(`:443`) with Traefik holding the ACME certificate; the server receives HTTP/1.1
on port 8765. The `Host` label is the client's `mcp.example.com` forwarded
unchanged, which the `0.0.0.0` bind accepts. Traefik copies the response body
with a 100 ms flush (`responseForwarding.flushInterval`, its default), skipping
that interval for a response it recognises as streaming, and its `writeTimeout`
defaults to `0` — no timeout — so the SSE stream is not cut off. **Do not attach
the `buffering` middleware** to this router, and do not attach `compress`
either: compressing `text/event-stream` holds the frames in the compressor's
buffer. If you bind a specific address instead of `0.0.0.0`, Traefik's only lever
is `passHostHeader: false`, which stops the client's `Host` from being forwarded
and leaves the address Traefik dials as the `Host`. With the Docker provider
that address is a container one, so a `127.0.0.1` bind cannot be satisfied this
way; declare the service in the file provider with `http://127.0.0.1:8765` if
that is the bind you want.

### Caddy

```yaml
services:
  caddy:
    image: caddy:2
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config
    networks: [mcp]

  bases-mcp:
    image: localhost/bases-mcp:latest
    command: ["--transport", "http"]
    environment:
      BASES_MCP_VAULT: /vault
      MCP_HTTP_HOST: "0.0.0.0"
      MCP_HTTP_PORT: "8765"
    volumes:
      - /path/to/vault:/vault:rw
    expose: ["8765"]
    networks: [mcp]

networks:
  mcp:

volumes:
  caddy-data:
  caddy-config:
```

`Caddyfile`:

```
mcp.example.com {
	reverse_proxy bases-mcp:8765 {
		flush_interval -1
	}
}
```

Caddy dials `bases-mcp:8765` and TLS terminates in Caddy at `:443` — it obtains
and renews the certificate on its own, so there is no certificate config here at
all. `Host` is forwarded unchanged by default, which the `0.0.0.0` bind accepts.
`flush_interval -1` means "flush after every write": Caddy already flushes
immediately for a `Content-Type: text/event-stream` response, which is what
rmcp's stream and its POST replies both are, but the explicit value is what
survives a change to that heuristic. Do not wrap this route in `encode` — gzip
buffers the frames.

### nginx

```yaml
services:
  nginx:
    image: nginx:1.27-alpine
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./nginx.conf:/etc/nginx/conf.d/default.conf:ro
      - ./certs:/etc/nginx/certs:ro
    networks: [mcp]

  bases-mcp:
    image: localhost/bases-mcp:latest
    command: ["--transport", "http"]
    environment:
      BASES_MCP_VAULT: /vault
      MCP_HTTP_HOST: "0.0.0.0"
      MCP_HTTP_PORT: "8765"
    volumes:
      - /path/to/vault:/vault:rw
    expose: ["8765"]
    networks: [mcp]

networks:
  mcp:
```

`nginx.conf`:

```nginx
server {
    listen 80;
    server_name mcp.example.com;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl;
    http2 on;                        # nginx >= 1.25.1; drop this line on older
    server_name mcp.example.com;

    ssl_certificate     /etc/nginx/certs/fullchain.pem;
    ssl_certificate_key /etc/nginx/certs/privkey.pem;

    location /mcp {
        # No URI part on proxy_pass: the request path must reach the server
        # unchanged. `proxy_pass http://bases-mcp:8765/;` would strip `/mcp`
        # and every request would 404.
        proxy_pass http://bases-mcp:8765;
        # The upstream leg is HTTP/1.1 (see below). The default was 1.0
        # before nginx 1.29.7.
        proxy_http_version 1.1;
        # Clear the Connection header nginx would otherwise send, so the
        # upstream connection is not closed after the first response.
        proxy_set_header Connection "";
        # Forward the client's Host. The 0.0.0.0 bind accepts any value;
        # on a loopback bind this must be `127.0.0.1` instead.
        proxy_set_header Host $host;
        # rmcp already sends `X-Accel-Buffering: no` on the stream; this is
        # belt and braces, and it also covers anything added later.
        proxy_buffering off;
        proxy_cache off;
        # SSE has no fixed length: let a quiet stream stay open.
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
}
```

nginx dials `bases-mcp:8765` and TLS terminates at `listen 443 ssl`, with the
certificate read from disk and renewed by whatever issued it — nginx does not
obtain certificates. `Host: mcp.example.com` is forwarded via `$host`, which the
`0.0.0.0` bind accepts; on a loopback bind the line must be
`proxy_set_header Host 127.0.0.1;`, because `Host: mcp.example.com` is exactly
what that bind refuses. Buffering is off for the SSE frames. Do not add
`text/event-stream` to `gzip_types`: nginx's default `gzip_types` does not
include it, and adding it would hold frames in the compressor. The stream is not
silent, either: rmcp sends an SSE keep-alive every 15 seconds (its default
`sse_keep_alive`, which this server does not override), so the 60 s default for
`proxy_read_timeout` would usually survive between keep-alives — raising it is
what stops a single late keep-alive from closing a session that is still live.

## What every proxy must forward

- **The path, unchanged.** The router is `nest_service(HTTP_PATH, service)` with
  `HTTP_PATH = "/mcp"` (`src/main.rs`, `src/transport.rs`), so the client URL is
  `https://mcp.example.com/mcp`. A rewrite that strips or prefixes the path
  reaches nothing.
- **All three methods.** `POST` carries a request, `GET` opens the SSE stream,
  `DELETE` closes a session (`src/main.rs`). A proxy that only allows `POST`/`GET`
  leaves sessions to expire instead of closing them.
- **Request headers, including `Mcp-Session-Id` and `MCP-Protocol-Version`.**
  The session id comes back on `initialize` and the client echoes it on every
  later request; a proxy that strips it makes every call a new session. `Accept`
  must survive too: rmcp answers `406 Not Acceptable` to a `POST` whose `Accept`
  does not include both `application/json` and `text/event-stream`, and to a
  `GET` that does not include `text/event-stream`.
- **The upstream leg in HTTP/1.1.** The server is built with axum's `http1`
  feature only (`Cargo.toml`), so it cannot speak HTTP/2 to the proxy. All three
  examples above dial HTTP/1.1. nginx needs it said out loud: `proxy_http_version`
  defaulted to `1.0` before nginx 1.29.7 and to `1.1` from 1.29.7 on, so the
  `nginx:1.27-alpine` image in the example above is HTTP/1.0 unless the directive
  is written, and HTTP/1.0 has no chunked encoding for the stream to travel on.
- **No buffering and no response compression.** Every response that matters is
  `text/event-stream` — rmcp sends it with `Cache-Control: no-cache` and
  `X-Accel-Buffering: no` (`rmcp-3.5.0`, `common/server_side_http.rs`), and the
  POST replies are SSE too, not a single JSON body. A buffering proxy holds a
  frame until the buffer fills and the client sees a connected stream that
  produces nothing.

## Timeouts and connection limits belong at the edge

**The listener configures none.** There is no header-read, idle, keep-alive or
request timeout anywhere in `src/`, and no knob to set one
(`src/main.rs` builds `axum::serve(...).with_graceful_shutdown(...)` and nothing
else). An unauthenticated client that opens connections and dribbles headers
holds them indefinitely, and with no connection cap that is a slowloris /
file-descriptor exhaustion exposure.

The proxy is the place to close it, because it is also where the network
boundary already is. What each one needs:

**nginx** — a header timeout and a connection cap:

```nginx
# http { } level
limit_conn_zone $binary_remote_addr zone=mcp_per_ip:10m;

location /mcp {
    # ... the proxy_pass block above ...
    client_header_timeout 10s;
    client_body_timeout   30s;
    send_timeout          1h;   # the SSE stream is long-lived; do not cut it
    limit_conn            mcp_per_ip 8;
    limit_conn_status     429;
}
```

`send_timeout` must stay long: the stream is supposed to idle between events, and
a short one is the "connects and produces nothing" failure this document already
warns about for buffering.

**Traefik** — `respondingTimeouts` on the entrypoint and a rate-limit
middleware:

```yaml
entryPoints:
  websecure:
    address: ":443"
    transport:
      respondingTimeouts:
        readTimeout: 30s
        idleTimeout: 1h
```

```yaml
http:
  middlewares:
    mcp-connlimit:
      inFlightReq:
        amount: 8
        sourceCriterion:
          ipStrategy:
            depth: 1
```

**Caddy** — `timeouts` on the server block and `request_body` limits:

```
{
    servers {
        timeouts {
            read_header 10s
            read_body   30s
            idle        1h
        }
    }
}
```

Caddy has no built-in per-IP connection cap; use `caddy-ratelimit` or put the cap
in front.

**These are starting points, not tuned values.** The numbers that matter are the
ones your clients tolerate: an MCP client holds an SSE stream open for the life
of a session, so `idle`/`send_timeout` must exceed the longest quiet period you
expect, and the connection cap must exceed the number of concurrent sessions one
agent opens.

## What TLS does not fix

TLS protects the bytes between client and proxy. It is **not authentication**:
the listener still has no credentials, no authorization and no rate limiting, and
the trust model is still "the network is safe" (D9 in `docs/divergences.md`).
Terminating TLS in front of it does not change that — the proxy forwards
whoever can reach it. Authentication belongs in the proxy (basic auth, client
certificates, whatever the deployment already runs) or in the server, and the
server does not have it yet.
