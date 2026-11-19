# Forgejo 17.0.0 for Linux amd64

This is the pinned Forgejo binary distributed unmodified alongside
**gewu-multiverse 0.1.42** (both inside the `x86_64-linux` release archive under
`forgejo/` as a separate executable aggregation and as a standalone release
asset).

- **Upstream version:** Forgejo v17.0.0 (release tag `v17.0.0`).
- **Source:** <https://codeberg.org/forgejo/forgejo/src/tag/v17.0.0>
  (`git clone --branch v17.0.0 https://codeberg.org/forgejo/forgejo.git`).
- **Upstream asset:** `forgejo-17.0.0-linux-amd64`.
- **SHA-256:** `5a3b07e124ed70b4e46ee3939515f003bd3cc85352257a9a4607a527251042ca`.
- **Signature:** `forgejo-17.0.0-linux-amd64.asc`, made by the Forgejo release
  signing subkey `3BF4 E813 F848 1241 1DA0 1E5B C418 6DF6 6F4B 6750` under the
  Forgejo primary key `EB11 4F5E 6C0D C2BC DD18 3550 A4B6 1A2D C592 3710`
  (`Forgejo <contact@forgejo.org>`).
- **Licence:** the GNU General Public License, version 3 or later; `LICENSE` is
  the upstream file at tag `v17.0.0`. `gewu-multiverse` itself remains licensed
  under `MIT OR Apache-2.0`; the two executables are separate programs in one
  archive (mere aggregation) and communicate only over local CLI/HTTP.
- **Unmodified:** the binary is byte-for-byte the upstream release asset. It was
  not rebuilt, patched, stripped or otherwise changed for this release.
- **Checked before publication:** `sha256sum -c forgejo-17.0.0-linux-amd64.sha256`
  and `gpg --verify forgejo-17.0.0-linux-amd64.asc forgejo-17.0.0-linux-amd64`
  both pass against the digest and key above.

The product pins the same digest in its source and checks every local companion
copy or download against it before renaming the file into place. When the
companion `forgejo/forgejo-17.0.0-linux-amd64` file from the release archive is
present, installation completes offline without any network request; otherwise
the fallback download sources are the matching GitHub release asset and the
official Codeberg release
`https://codeberg.org/forgejo/forgejo/releases/download/v17.0.0/forgejo-17.0.0-linux-amd64`.
