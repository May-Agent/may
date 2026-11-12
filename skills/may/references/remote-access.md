# Remote access

Remote access makes the web UI and API reachable from the user's other devices over Tailscale. It needs the `tailscale` CLI installed and signed in on the machine running the daemon.

## How it works

- With `[remote_access] enabled = true`, the daemon runs `tailscale serve --bg --https=<https_port>` in front of its remote listener (a loopback listener that serves the same routes as the local one but demands an identity). Devices on the tailnet open `https://<machine MagicDNS name>` (`:<port>` appended unless the port is 443). Turning it off runs `tailscale serve --https=<port> off`.
- A running serve is re-checked every 30 seconds against `tailscale serve status` and restored if gone.
- Settings > Remote access in the web UI shows the state (Disabled, Starting, Running, Error), the public URL, last error and recent logs, and has Start tunnel / Stop tunnel and the allowed logins list.

## Who gets in

Every remote request needs an identity or gets 401, unmatched paths and tunnel pages included. Through `tailscale serve`, the `Tailscale-User-Login` header is trusted only when the login is accepted: the owner of the tailscale node (always, when it can be resolved) plus `[remote_access] allowed_logins`. Users of shared devices are not accepted unless listed. If the owner cannot be resolved only listed logins get in; with none, nobody does. A request that came through Funnel is never trusted by login. A non-loopback address — one added in `[daemon] listen` or given to `may rest --host` (`0.0.0.0` included) — identifies each peer through tailscaled's `WhoIs` and accepts the same logins; it answers only a request whose `Host` names the node itself — a tailnet address, the `MagicDNS` name (any case) or the address the connection arrived on — and refuses any other `Host` with 403 before resolving an identity, so a DNS-rebinding page cannot act as the user. The node's names come from `tailscale status --json` and are re-read when a `Host` does not match. On both, an identified request whose `Origin` is present and is not the origin it was addressed to gets 403 (`null` included), so a page on another site cannot act as the user: through `tailscale serve` that is `https://<Host>` (`http://` over plain-HTTP serve), on a direct address `http://<Host>`. A request without `Origin` (curl, a script, a same-origin `GET`) is decided by its identity alone. The unix socket `~/.may/daemon.sock` trusts the machine's own user. A loopback address (`127.0.0.1:<[daemon] port>`) trusts it too, but refuses with 403 a request whose `Host` is not a loopback name (`localhost`, `*.localhost`, `127.x.x.x`, `[::1]`) or whose `Origin` is present and not a loopback http(s) origin (`null` included), so other web pages cannot drive the API.

## Local profiles block it

Remote access refuses to start while any local-environment profile exists, and the three built-in profiles are local. The error reads "remote access is blocked while local-environment profiles exist; enable allow_local_environment to override". Setting `[remote_access] allow_local_environment = true` lifts this (the user is then exposing the host to everyone accepted above). Creating or editing a profile re-evaluates this.

## Funnel (public webhooks)

`[remote_access] funnel = true` runs `tailscale funnel` for `/api/tasks/webhook` only, on `funnel_port` (443, 8443 or 10000; default 443), through a separate loopback listener whose router holds the webhook routes and nothing else. It must differ from `https_port` while remote access is enabled (Funnel exposes its whole port), and it is blocked by local profiles in the same way. Deliveries authenticate by token and signature; bodies larger than `webhook_max_body_bytes` (default 25 MiB) are refused with 413.

## Settings

`[remote_access]`: `enabled` (false), `tailscale_binary` (`tailscale`), `https_port` (443), `allow_local_environment` (false), `allowed_logins` (empty), `funnel` (false), `funnel_port` (443), `webhook_max_body_bytes` (26214400). Changes apply live.
