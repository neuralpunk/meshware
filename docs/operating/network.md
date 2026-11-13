# Network

*Reference. Describes what is in the tree.*

**The page to send a firewall team.** Every port meshware uses, which direction it is opened
in, and what breaks if it is closed.

---

## 1. Ports

| Port | Protocol | Direction | Default | Required |
|---|---|---|---|---|
| **7777** | **UDP** (QUIC) | node ↔ node, both ways | all interfaces | **Yes.** This is the mesh |
| 5353 | UDP (mDNS) | LAN multicast | on | No — pairing discovery on a LAN (`meshnode pair`, Director's *Join a mesh on this network*) |
| 7778 | UDP (broadcast) | LAN broadcast, received by the member letting a machine in | while a machine waits to pair | No — the pairing beacon, the same discovery for networks that drop multicast |
| 7776 | TCP (HTTP) | inbound | **loopback only** | No — `/healthz` and `/metrics` |
| 8600 | UDP (DNS) | inbound | **loopback only** | No — answers `<name>.mesh`; `[network] dns_bind` exposes it deliberately (§7) |
| none | TCP (HTTP) | inbound | **not bound at all** | No — the HTTP API. There is no default port: it is off until `[api] listen` names an address |
| 20000–32000 | whatever the job speaks | inbound, from clients and load balancers | `[network] dynamic_ports` | Only for a service something off its machine should reach — §7 |

**There is one required port and it is UDP.** This surprises people: meshware's transport is
QUIC, so a firewall that allows TCP 7777 and nothing else will produce a mesh where every node
looks permanently unreachable. `meshnode doctor` says so explicitly rather than leaving you to
find it.

Nothing listens on TCP 7777. The node itself needs no TCP inbound unless you turn on the
metrics endpoint or the API; the jobs it runs are another matter, and §7 is about them.

**UDP 7777 is safe to leave reachable, and it is bounded for anyone who is not a member.** A
peer outside the mesh gets:

- 10 seconds to finish its handshake;
- at most 256 handshakes in flight at once;
- 512 open connections in total, 32 from any one address;
- 4 streams per connection, and 256 KiB per request.

Members are held to none of these. A probe (`meshnode ping`) from outside the mesh learns that
the node is in a mesh, but not which mesh, and not its name.

**The HTTP API, when it is on, has two requests anybody with a key can make:** asking to enrol, and
starting a sign-in through an identity provider. Both are limited per client address (an IPv6
client by its /64) and per node, and a limited request is answered `429`:

- enrolment: 10 per address, then 1 a minute; 100 across the node, then 30 a minute; and none
  at all while 500 requests are already waiting for an admin;
- sign-in: 20 per address, then 10 a minute; 100 across the node, then 60 a minute. A sign-in is
  polled only by the key that started it, on the node that started it, at most once a second.

The node's own socket counts as one more address. Callers who hold a role are not limited when
signing in.

## 2. Outbound

A node makes **no outbound connection you did not configure**. There is no telemetry, no
licence check and no update ping.

The complete list of things that can dial out, all of them off or on-demand:

| What | When | Where |
|---|---|---|
| Mesh traffic | Always | Other nodes, UDP 7777 |
| `meshnode upgrade --self` | Only when you run it | The release channel, or `[release] base` |
| History export | Only if an admin signed a sink | Wherever the sink points |
| `meshnode login` | Only when somebody runs it | The configured identity provider |
| Notifications | Only if `[notify]` is set | Your webhook / ntfy |

An air-gapped mesh is a supported configuration. See [air-gap.md](air-gap.md).

## 3. What a mesh needs from a network

**Every node needs to reach every other node on UDP 7777.** meshware is a peer mesh: there is
no node that relays for the others by default, and a partial mesh behaves like a partitioned
one.

Where that is not possible — NAT, CGNAT, a laptop at home — the WAN path handles it:

- **Address discovery and hole punching.** Two NATed nodes coordinate a simultaneous send and
  usually establish a direct path.
- **Relays.** A node with a public address and `[wan] relay = "on"` will carry traffic for
  peers that cannot punch. **The relay sees ciphertext only** — the inner QUIC session between
  the two nodes is an ordinary key-pinned connection that happens to cross a third party.

```console
# on a node with a public address:
$ meshnode up --advertise 203.0.113.5:7777      # + [wan] relay = "on" to help NATed peers
$ meshnode invite create                         # the token carries the rendezvous address

# behind CGNAT:
$ meshnode join mwj1…  && meshnode up
```

## 4. Overlays

meshware works over Tailscale, WireGuard, ZeroTier and ordinary VPNs with no special
configuration: an overlay address is just another candidate address. If the overlay is the only
route, pass `--via` when joining so the joiner dials the address that actually works rather
than the one the minting node advertised.

## 5. Proxies

**meshware's mesh traffic cannot go through an HTTP proxy.** It is QUIC, not HTTP. A corporate
proxy is not on the path and does not need to be.

The outbound HTTPS calls in §2 — release fetches, sinks, `login` — are ordinary HTTPS and
respect the system trust store. There is no proxy-configuration setting today; if you need one,
point `[release] base` at an internal mirror instead.

## 6. IPv6 and dual stack

The listener binds what you tell it. On a dual-stack host, this serves both families:

```toml
[listen]
addr = "[::]"      # brackets included: the node joins this and the port as "[::]:7777"
port = 7777
```

`listen` is a table, not an address — `listen = "[::]:7777"` is refused when the config loads. A node listening on an unspecified address advertises every address on every
interface — both families, ranked with its default route first — and peers dial the candidates
in parallel, so a mixed v4/v6 mesh converges on whatever works. An IPv6-only host is an
ordinary case, not a configuration.

Three kinds of address are never advertised, because a peer cannot use them: link-local
(`fe80::/10` and `169.254/16` — an IPv6 zone index means something different on the machine
reading it), the unspecified address itself, and **any address in a family the listener does not
serve** — a node with `[listen] addr = "0.0.0.0"` advertises no IPv6, because nothing on it is listening
there. Loopback always is, in whichever family applies.

At most eight addresses are advertised, best first, because each one is a staggered dial
attempt on every peer that wants to reach this node. On a machine with more than that — a build
host with a `veth` per container — name the interfaces mesh traffic belongs on with
`[network] advertise_interfaces`.

If a whole VLAN is renumbered at once, every recorded address becomes stale in every direction
and no node can reach another to announce the change. That is the one case the self-healing
path cannot absorb; `meshnode nodes addr <node> --set HOST:PORT` repairs it, one node at a time.

## 7. Reaching a service

**A copy of a service answers on the machine it runs on, at the port that machine gave it.**
There is no proxy, no virtual IP and no ingress in between. This is Nomad's model, and it draws
the same line: meshware places the work, allocates the port and tells you where it is; getting
traffic to it from a name like `https://example.com` is a load balancer's job.

### To look at it

```console
$ meshnode registry web
service:  web
resolves: 2 of 2 instance(s)

NODE             ADDRESS                LABEL      HEALTH     INSTANCE
forge            192.168.1.31:24112     http       passing    …
edge-7           192.168.1.40:20417     http       passing    …

$ curl http://192.168.1.31:24112/
```

Director shows the same list under **Services › Addresses**, one row per copy, with the
address to copy, a checklist of what else has to be true, and the nginx, HAProxy, Caddy or
Traefik configuration for those copies, ready to paste. `<name>.mesh` answers with the same machines (A/AAAA) and, for SRV, their ports.

### Where a job should listen

| The job binds | Reachable from |
|---|---|
| `0.0.0.0`, or `$MESHWARE_IP_<LABEL>` / `$MESHWARE_ADDR_<LABEL>` | Anywhere that can reach the machine on that port |
| `127.0.0.1` | Its own machine only |

`MESHWARE_IP_<LABEL>` is the node's listen address — `0.0.0.0` on a node listening on every
interface, which is every ordinary node — and `MESHWARE_ADDR_<LABEL>` is that address with
`MESHWARE_PORT_<LABEL>`. A container that declares `to` has its port published on every
interface of the machine; one that does not runs on the host network and binds
`MESHWARE_PORT_<LABEL>` itself.

**The port has to be open from wherever the client is.** meshware opens no firewall for a job:
`meshnode adopt` opens UDP 7777 in the firewall and nothing else. Open the dynamic range (or
the one static port) to the network your clients or load balancer sit on.

### A name for other people: a load balancer

Put nginx, HAProxy, Caddy or a cloud load balancer in front, pointed at the machines. There are
two shapes, and the first is the one to start with.

**A fixed port on every machine.** Give the port `static: 8080` and run the deployment with
`count: system` (one copy per matching machine) or `distinct_hosts`. Every machine that runs a
copy answers on 8080, so the load balancer's list is a list of machines, and it changes when
machines do rather than when copies move:

```nginx
upstream web {
    server 192.168.1.31:8080;
    server 192.168.1.40:8080;
}
server {
    listen 80;
    server_name example.com;
    location / { proxy_pass http://web; }
}
```

**Dynamic ports.** A load balancer that reads DNS SRV records can follow copies wherever they
land: `web.mesh SRV` carries each healthy copy's address and port. It has to be able to ask a
node's responder, which listens on loopback only unless `[network] dns_bind` says otherwise —
set it to an address the load balancer can reach, deliberately.

```console
$ dig @192.168.1.31 -p 8600 web.mesh SRV
```

Either way, **give the load balancer its own health check.** `.mesh` stops handing out a copy
the moment its check fails, but a static upstream list does not know that.

### The mesh proxy is gone

Until 2026-09-14 every registered name also had an address of its own on each member machine
(`127.77.x.y`) where the node listened on the declared port and forwarded through the mesh, so
`http://web.mesh` worked on port 80. It was removed: it duplicated what a load balancer does,
less well, and needed a low-port capability to do it.

- `[network] proxy` and `proxy_prefix` still load, and are ignored with a warning.
- `meshnode adopt` (and Director's *Add machines*) removes the `meshnode.service.d/proxy.conf`
  capability drop-in it used to install; `sudo meshnode dns install` removes that drop-in and the port-floor sysctl
  (`/etc/sysctl.d/60-meshware-proxy.conf`), and puts `ip_unprivileged_port_start` back to 1024.
- A node on an older build still runs its proxy. A connection it sends through a node on this
  build is refused by name, and it falls back to dialling the instance directly — which works
  wherever the instance's port is open.

## 8. Checking it

```console
$ meshnode doctor              # connectivity, clock skew, ports, sandbox, DNS
$ meshnode ping <node>         # handshake + round trip to one peer, through the mesh
$ meshnode nodes addr web-07   # what the mesh thinks that node's addresses are
```

`doctor` is the one to run first and the one to paste into a support request.
