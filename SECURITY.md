# Security policy

OpenRDP gives remote control of a Mac, so security issues matter a lot here.

## Reporting a vulnerability

Please **do not open a public issue**. Report it privately through [GitHub's private vulnerability reporting](https://github.com/org-event/OpenRDP/security/advisories/new) instead. Include what is affected, how to reproduce it, and what an attacker could do with it. You should get a reply within a week.

## Supported versions

Only the latest release and `main` get fixes.

## Deployment advice

- Do not expose port 3389 to the internet. Reach the Mac through a VPN such as WireGuard, or through an SSH tunnel.
- Use a long, unique server password. It is stored in the LaunchAgent plist, readable only by your user.
- The default TLS certificate is self-signed. Clients cannot tell it apart from an attacker's certificate the first time they connect, so check its fingerprint, or pass your own with `--cert` and `--key`.
