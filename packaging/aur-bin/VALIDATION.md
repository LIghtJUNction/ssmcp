# Validation record — 2026-11-24

Recipe added from repository base `5daf52547c0402e78aa0a6a277d855b399e3308a`.
The authoritative recipe and `.SRCINFO` are the files in the accompanying PR.
The pinned release is [v0.10.31](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.31),
whose annotated tag resolves to source commit
`2ca0fa6ac8c7318a7f8b6c83c97c69d8d1266705`.

Input file SHA256:

- `PKGBUILD`: `96ab6dbfdda1ad52f53feaa8828c8ae9bf75f6ba27f81f1f85f94832f712dcee`
- `.SRCINFO`: `a8e4d64467791072129eb5bcb4c0e1505651b59c4f21b8722c3877eb9f10efc4`

## Clean Arch x86_64 build and installation

Used a new official `archlinux:base-devel` container pinned to digest
`sha256:949ecb1661609ab7e8f6854ba03efd62792e7a796313738e94b040c095c20d91`.
The container was updated using official Arch repositories with pacman signature
validation enabled. The cloud environment's existing public CA certificates
were added only inside that disposable container for its TLS-intercepted network;
TLS checks and package signatures were not disabled. No home directory, SSH key,
account credential or running relay was mounted into it.

The image normally excludes manual/documentation files with `NoExtract` to save
space. Those exclusions were removed inside the test container so installation
could verify the **entire** package, including its manuals and README. No host
configuration was changed. This is a clean container build, not a claimed
`extra-x86_64-build` devtools/nspawn run.

As an unprivileged `builder` account:

```sh
makepkg --printsrcinfo
makepkg --cleanbuild --noconfirm
namcap PKGBUILD ssmcp-bin-0.10.31-1-x86_64.pkg.tar.zst
```

The package built successfully with source integrity checks and dependency checks
active. No `cargo` executable was installed or invoked. `.SRCINFO` was generated
with makepkg and compared byte-for-byte with the repository file.

Actual disposable-container installation checks passed:

```text
pacman -U <package>: success
pacman -Qkk ssmcp-bin: 16 total files, 0 altered files
pacman -T 'ssmcp=0.10.31': satisfied
ordinary-user ssmcp --version: ssmcp 0.10.31
ordinary-user ssmcpd --version: ssmcpd 0.10.31
ldd on both installed executables: no missing libraries
```

Installed executable SHA256 values match the uncompressed official archive bytes:

- `ssmcp`: `bcfd47dfde3764f297054d2039b1f75833d5c664970dc65cc1a5f30e945df388`
- `ssmcpd`: `c7173738a397f1ba998ddc738d5bf45970398908a5f74ad2e5b9b2c22017c6d7`

The archive resolves `libgcc_s.so` to `libgcc_s.so=1-64` in package metadata.
Package contents include no `/etc` files, systemd activation entries or install
script. No service was started.

## Negative controls and lint

- With the binary package installed, pacman rejects a disposable synthetic local
  `ssmcp-git` package as conflicting and retains `ssmcp-bin`.
- With that synthetic package installed instead, pacman rejects `ssmcp-bin` as
  conflicting. The synthetic package is only a local transaction probe; nothing
  was submitted to or taken from the real `ssmcp-git` AUR repository.
- A deliberately altered archive in a separate throwaway build directory fails
  `makepkg --verifysource` with a SHA256 failure. The official source cache and
  pinned checksums were not changed to make it pass.
- `bash -n PKGBUILD` and `git diff --check` pass.
- Namcap reports no errors. Its five warnings are reviewed: one requests `$CARCH`
  for the intentionally architecture-specific official upstream filename; four
  concern runtime programs (Node.js, Bubblewrap, util-linux and Git) which are
  required via subprocesses and cannot be inferred from ELF dynamic libraries.
  No warnings were hidden or used to remove required runtime dependencies.

## ARM64 limits and publication blocker

The ARM64 official archive was independently SHA256-verified and packaged using
makepkg with an aarch64 configuration on the x86_64 container. Metadata reports
`arch = aarch64`; readelf reports the packaged executable as AArch64. There is no
compilation or target executable invocation in `package()`.

**Native ARM64 execution and installation were not run.** This environment has
no ARM64 host/emulator. Cross-packaging and checksum verification are not a claim
of native ARM64 runtime validation.

AUR RPC reported `ssmcp-bin` free and `ssmcp-git` maintained by `lightjunction`.
The cloud session has no SSH authentication agent and no independently verified
AUR maintainer session. Consequently **no AUR push or package registration was
performed**. The reviewed source can be submitted from an existing authorized
maintainer environment as described in README; private keys must not be copied
into this workspace. Generated test packages remain temporary cloud artifacts,
not committed source or a newly published GitHub Release.
