# Networking and egress

Every sandbox has its own network namespace, bridged to the host by [`pasta`](https://passt.top/), which runs on the host as your user. What the sandbox can reach is decided by one configuration key, `egress`, and the databases you declare under `network`.

## Open egress (the default)

With `egress` absent or `true`, the sandbox reaches whatever your host reaches. Nothing is filtered and no proxy runs. This is what most agents need out of the box: they talk to their model provider directly.

- **Name resolution** works: hort writes an `/etc/resolv.conf` into the sandbox naming `198.51.100.53`, and pasta answers that address by forwarding the query to your host's resolver.
- **Services on the host's loopback are reachable** at `127.0.0.1` inside the sandbox. A development server or database listening on `127.0.0.1:5432` on your host answers on `127.0.0.1:5432` inside the box, with no configuration. Closing that interface without changing the whole posture is [planned](roadmap.md#closing-the-hosts-loopback-to-an-open-sandbox).
- **No proxy variables** are set.

Open egress does not prevent exfiltration. A hostile repository could send data anywhere, including to services on your host's loopback. See the [Security model](security.md).

## An egress allowlist

```jsonc
{
  "egress": { "allow": ["api.anthropic.com", "github.com", "*.githubusercontent.com"] },
}
```

With an allowlist the sandbox has **no route to anything** except two kinds of endpoint on its own loopback:

1. an HTTPS proxy that hort runs on the host for this sandbox, which only tunnels to hosts on the list, and
2. the databases you declared under `network`.

Everything else fails: a raw connection to an IP address, UDP and QUIC, DNS queries, a host port you did not declare. There is no `/etc/resolv.conf`; the proxy resolves host names itself.

### How entries match

- A **bare name** such as `github.com` matches exactly that host and nothing else, not even `www.github.com`.
- A **`*.` entry** such as `*.githubusercontent.com` matches any subdomain (`raw.githubusercontent.com`) but **not** the name itself. List the apex separately if you need it.
- Matching ignores case and a trailing dot. `notexample.com` never matches `*.example.com` or `example.com`.
- An entry must be a plain host name: no scheme, no port, no path. `"https://github.com"` makes `hort up` fail with `invalid name`.

`"egress": false` is an **empty allowlist**: no host is reachable through the proxy, while declared databases still are.

### Tools must use the proxy

In every session of an allowlisted sandbox, hort sets:

```text
HTTP_PROXY=http://127.0.0.1:<port>     http_proxy=http://127.0.0.1:<port>
HTTPS_PROXY=http://127.0.0.1:<port>    https_proxy=http://127.0.0.1:<port>
ALL_PROXY=http://127.0.0.1:<port>      all_proxy=http://127.0.0.1:<port>
NO_PROXY=127.0.0.1,localhost           no_proxy=127.0.0.1,localhost
```

A tool that honours these variables works for allowed hosts. A tool that ignores them finds no route and fails. This is **fail-closed by design**.

The proxy only opens **`CONNECT` tunnels**, which is what clients use for `https://` URLs. It checks the host named in the request against the list, then checks that the TLS handshake names the same host, and never decrypts anything. A plain `http://` request sent to the proxy is refused, because cleartext names nothing the proxy can confirm.

`NO_PROXY` covers `127.0.0.1` and `localhost` so that database connections go straight to their forward. Some tools ignore `NO_PROXY` (BusyBox `wget` is one) and send even local requests to the proxy, which refuses them.

### Reading the proxy log

Every decision the proxy makes is appended to the sandbox's log on the host:

```bash
grep -E '^(allowed|refused)' "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/hort/sandboxes/fix-login/output.log"
```

```text
allowed api.anthropic.com
refused evil.test (not in the allowlist)
refused http://example.com/ (only CONNECT tunnels are proxied)
```

This makes a failing connection a one-look diagnosis:

- a `refused <host> (not in the allowlist)` line: the allowlist blocked it; add the host if you trust it;
- a `refused ... (only CONNECT tunnels are proxied)` line: the tool sent a plain HTTP request; use `https://`, or for a local address make sure the tool honours `NO_PROXY`;
- `refused <host> (the connection asked for <other>)`: the TLS handshake named a different host than the request, which the proxy never allows;
- **no line at all**: the tool ignored the proxy variables and tried to connect directly, which has no route.

### What the agent cannot change

An allowlisted sandbox's network namespace is owned by hort, not by the sandbox. The agent is root only inside its own user namespace and holds no network privileges over the namespace it uses, so it cannot add a route, change addresses or remove the restriction:

```console
# ip route add default via 10.0.0.1
RTNETLINK answers: Operation not permitted
```

On kernels with Landlock ABI 4 or later (Linux 6.7+), sessions are additionally restricted by the kernel to connecting only to the proxy's and the declared databases' ports. Below that, `hort up` warns:

```text
warning: this kernel cannot restrict which ports a process connects to, so the egress allowlist of this sandbox runs without its kernel layer (Linux 6.7 or newer enforces it)
```

The other layers still hold. An allowlisted sandbox also needs `ip` (iproute2) on the host, to empty the namespace's route tables.

### Changing the posture of a running sandbox

The network is wired when the sandbox is built. Editing `egress` in the configuration does not change a sandbox that is already running; `hort down <name>` and `hort up <name>` to rebuild it with the new posture. Commit what you want to keep first.

## Databases

Declare each database your project needs under `network`:

```jsonc
{
  "network": [
    { "mode": "host", "host": "127.0.0.1", "port": 5432 },
    { "mode": "network", "host": "192.168.1.20", "port": 6379 },
  ],
}
```

Inside the sandbox, **every declared database is reached at `127.0.0.1:<port>`**, in both postures. Point your application's development configuration there.

- `host` is the address the database answers on, as seen from your host. For a database installed on the host that is `127.0.0.1`. For one in a container (for example Docker Compose), use the address and port it is published on.
- `port` is its port, and also the port the sandbox uses.
- `mode` (`"host"` or `"network"`) is informational; both are handled the same way. hort does not join Docker networks.

A database on the host's loopback is reached directly. For any other address, hort starts a small forwarder on the host listening on `127.0.0.1:<port>` and relaying to the declared address, so that port must be free on your host's loopback.

Under an allowlist, only declared databases are reachable. Under open egress, services on the host loopback are reachable anyway, and declaring a database on another address is what makes it appear at `127.0.0.1:<port>`.

Two databases declared on the **same port** at different addresses cannot both be reached, because inside the sandbox both would be `127.0.0.1:<port>`. `hort up` refuses such a configuration before it builds or changes anything:

```text
two databases are declared on port 5432 (10.0.0.5 and 10.0.0.6), and a sandbox can reach only one of them — remove one from "network" in your configuration or give it another port
```

Give every declared database its own port.

Only development databases and credentials belong in a sandbox. Never production.
