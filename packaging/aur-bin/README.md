# Official prebuilt AUR recipe

`ssmcp-bin` packages the **v0.10.31** GitHub Release, not a Git checkout. It installs
`ssmcp`, `ssmcpd`, their manual pages, README and MIT license. There is no Cargo,
Rust, `build()` function, install hook, account creation or service activation.
The existing source-build recipe in `packaging/aur` is unchanged.

| Arch package architecture | Official release archive | SHA256 |
| --- | --- | --- |
| x86_64 | ssmcp-x86_64-unknown-linux-gnu.tar.gz | `7320249b42fbcda02f1b3f8188f2ee226ae2ecf793bf662f561e0c36cec0a5ab` |
| aarch64 | ssmcp-aarch64-unknown-linux-gnu.tar.gz | `b022546ff5f62af6a893961f79ff0a142aa3d50d7e3f3f02ce5a70bfb8a7144a` |

Both hashes were checked against the downloaded archives, the release's
`SHA256SUMS`, and GitHub's asset digests. Downloads are versioned, with no `SKIP`.
`!strip` and `!debug` preserve the official executable bytes.

`provides=('ssmcp=0.10.31')` satisfies consumers of the upstream package name.
`conflicts=('ssmcp' 'ssmcp-git')` prevents simultaneous installation of the source
and binary variants; there is no `replaces` entry or automatic takeover.

## Build and install

Review `PKGBUILD`, then run as an ordinary user on a fully updated Arch system:

```sh
makepkg --verifysource
makepkg --cleanbuild --syncdeps --install
```

The `libgcc_s.so` dependency is resolved by makepkg to the ELF SONAME/ABI, so
either the current split libgcc package or its compatible older provider can
satisfy it. Node.js, Bubblewrap, util-linux and Git are runtime dependencies used by the
client, even though ELF-only linters cannot infer these subprocess requirements.
Code Mode still requires the normal Linux namespace support; packaging does not
change that boundary. No relay credentials or running service are needed to
build the package.

See [VALIDATION.md](VALIDATION.md) for the actual x86_64 clean-container install,
ARM64 cross-packaging evidence, lint warnings and the native ARM64 limitation.

## Updating

Select the latest **published, non-prerelease** upstream GitHub Release. Update
`pkgver`, reset `pkgrel` to 1, and independently verify both archive SHA256 values
against that release before changing the checksum arrays. Do not calculate a new
checksum from an unverified replacement merely to make a build pass. Regenerate:

```sh
makepkg --printsrcinfo > .SRCINFO
```

Revalidate the build, installed version, binary identity, package dependencies
and conflicts. Never upload the downloaded binaries or package archives to AUR.

## AUR publication status and handoff

At the 2026-11-24 lookup, the public AUR RPC reported `ssmcp-bin` unregistered.
The existing `ssmcp-git` belongs to `lightjunction`; this recipe does not modify
that package or claim its maintainer credentials.

This cloud session has no SSH agent and no verified authenticated AUR identity.
The recipe is therefore **not published** by this change. Do not copy a private
key into the workspace. From an already authorized maintainer environment:

1. Recheck that `ssmcp-bin` is free or belongs to your account. Stop if a third
   party owns it; do not overwrite or adopt it as part of this task.
2. Authenticate with your existing AUR SSH setup (`ssh -T aur@aur.archlinux.org`)
   and confirm the greeting identifies the authorized maintainer account.
3. Clone `ssh://aur@aur.archlinux.org/ssmcp-bin.git` into a new directory. Copy
   only the reviewed `PKGBUILD` and generated `.SRCINFO` from this directory.
4. Run `makepkg --verifysource` and check `.SRCINFO` against `makepkg --printsrcinfo`.
   After the applicable build/install checks pass, commit those two source files
   and push using that environment's existing AUR authorization.

Do not run these publication commands from an account whose ownership has not
been confirmed, and do not use the old source-build package's AUR repository.
