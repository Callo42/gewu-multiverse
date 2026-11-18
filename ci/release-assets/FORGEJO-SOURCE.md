# Forgejo 17.0.0 for Linux amd64

This is the pinned Forgejo binary shipped as a release asset by
**gewu-multiverse 0.1.35**.

- **Upstream version:** Forgejo v17.0.0 (release tag `v17.0.0`).
- **Source:** <https://codeberg.org/forgejo/forgejo/tree/v17.0.0>
  (the same tag and source are mirrored at
  <https://github.com/forgejo/forgejo/tree/v17.0.0>).
- **Upstream asset:** `forgejo-17.0.0-linux-amd64`.
- **SHA-256:** `5a3b07e124ed70b4e46ee3939515f003bd3cc85352257a9a4607a527251042ca`.
- **Signature:** `forgejo-17.0.0-linux-amd64.asc`, made by the Forgejo release
  signing subkey `3BF4 E813 F848 1241 1DA0 1E5B C418 6DF6 6F4B 6750` under the
  Forgejo primary key `EB11 4F5E 6C0D C2BC DD18 3550 A4B6 1A2D C592 3710`
  (`Forgejo <contact@forgejo.org>`).
- **Licence:** the GNU General Public License, version 3 or later; `LICENSE` is
  the upstream file at tag `v17.0.0`.
- **Unmodified:** the binary is byte-for-byte the upstream release asset. It was
  not rebuilt, patched, stripped or otherwise changed for this release.
- **Checked before publication:** `sha256sum -c forgejo-17.0.0-linux-amd64.sha256`
  and `gpg --verify forgejo-17.0.0-linux-amd64.asc forgejo-17.0.0-linux-amd64`
  both pass against the digest and key above.

The product pins the same digest in its source and checks every download against
it before renaming the file into place. The first download source is this
release's own asset at
`https://github.com/Callo42/gewu-multiverse/releases/download/v0.1.35/forgejo-17.0.0-linux-amd64`;
the second is the official Codeberg release
`https://codeberg.org/forgejo/forgejo/releases/download/v17.0.0/forgejo-17.0.0-linux-amd64`.
