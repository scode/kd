# Router trust after the eMudhra incident

The Rust client dependencies and CLIProxyAPI pin include the available fixes. **The pinned codex-lb image is still
affected.** This change does not make an existing router deployment safe merely by installing a newer kd.

## CLIProxyAPI

The pinned v8.0.36 image index is `sha256:bc25ea627f675e4e3714f48d3b1dd22643e45464dcc1c3c7c5688f51bc6b49fb`. Its Linux
amd64 and arm64 executables both identify Go 1.26.10 and contain all eight intermediate-key hashes from that release's
[crypto/x509 distrust list](https://github.com/golang/go/blob/go1.26.10/src/crypto/x509/verify.go). This includes the
[Go mitigation for CVE-2026-111943](https://github.com/golang/go/issues/82345).

The image still contains Debian `ca-certificates 20250419~deb12u1`; it has not acquired a fresh system CA bundle. The
verified protection is Go's rejection of the compromised intermediate keys, including on uTLS paths that use
`crypto/x509`. Do not infer that unrelated clients inside the image have corrected trust from its application version.

This is a v7-to-v8 image change. The management endpoints used by kd, their authentication-file fields, and the
session-affinity configuration remain present in the tagged v8.0.36 source. Local kd tests use fixtures rather than a
live account migration; that is not evidence that every existing router configuration or account has been migrated.
Review upstream migration guidance and back up router state before a separately authorized deployment.

## Remaining codex-lb blocker

As checked on 2026-10-20, stable v1.24.0 still resolves to
`sha256:ba5598aaa70f7acf74a958139037604621ec2582a7fb5fe1e870f08bece807d6`. The inspected amd64 image contains certifi
2026.07.22 and Debian `ca-certificates 20250419`. Its
[HTTP context builder](https://github.com/Soju06/codex-lb/blob/84fde5a1ed5e0d5a58ccb3ec4b82938b059bf8c5/app/core/clients/http.py#L104)
loads the system defaults and then adds certifi. Both stores contain incident roots. Updating host certificates,
restarting the same image, or setting an additional CA file does not remove certifi's explicit trust.

A replacement must provide certifi 2026.10.19 or later and corrected effective system roots, or another verified
intermediate-key rejection on every affected TLS path. The
[Python incident notice](https://discuss.python.org/t/information-for-python-users-regarding-emudhra-emsign-ca-certificate-security-incident/109576)
also identifies uv 0.13.4 and pip 26.2.2 as fixes for their separately embedded bundles. The pinned image was built with
uv 0.12.5; uv is build-only, and pip is removed from its runtime. Its current upstream main still locks old certifi and
uses uv 0.13.1, so neither that source nor an uninspected newer tag is a verified replacement.

No corrected stable codex-lb image was available for this pin update. A separately reviewed downstream image could
correct both stores, but no such image is published or substituted here. Keep this blocker open rather than declaring
the router deployment remediated. Once a replacement is verified, update its digest and deploy it explicitly; changing
kd's lockfile cannot rewrite an existing container.
