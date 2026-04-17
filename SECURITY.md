# Security policy

## Reporting a vulnerability

Please report suspected security issues **privately** via GitHub's built-in
[Security Advisories](https://github.com/mlamp/lightorrent/security/advisories/new)
rather than opening a public issue or pull request. This keeps users safe
while a fix is in flight.

When reporting, include:

- A description of the vulnerability and the impact.
- Steps to reproduce, or a proof-of-concept if you have one.
- The affected version(s) / commit range.
- Any suggested remediation.

I aim to acknowledge reports within 7 days and resolve or status-update
within 30 days. A CVE will be requested for any issue that warrants one.

## Scope

In scope: the `lightorrent` crate, its HTTP API (`/api/v2/*`), the
published Docker image, and the release artifacts produced by GitHub
Actions. Supply-chain concerns in direct dependencies are in scope in
the sense that I will coordinate upstream fixes — dependency CVEs are
normally reported directly to the upstream project.

Out of scope: security issues in Sonarr/Radarr or other clients that
speak the qBittorrent API; issues that only manifest with
`api_bind_address` exposed to the public internet without a reverse
proxy or firewall.

## Hardening defaults

- The Dockerfile runs as non-root user `lightorrent:10001`.
- Recommended runtime flags: `--read-only --cap-drop=ALL --security-opt no-new-privileges`.
- `api_password` accepts PHC-encoded Argon2id hashes (`$argon2id$…`); use
  `lightorrent hash-password <plaintext>` to generate one.
- Binding to `0.0.0.0` is the default for container ergonomics — put it
  behind a reverse proxy, a VPN, or a firewall for anything other than
  a trusted LAN.
