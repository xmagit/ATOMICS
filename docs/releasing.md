# Signing and publishing a release

A `v<version>` tag matching `workspace.package.version` in `Cargo.toml` runs
`.github/workflows/web.yml`:

1. `preflight` checks that the tagged commit is on `main` and passed CI there.
2. `bundle` builds the web client. The two `desktop` jobs build on Ubuntu 22.04 (x86_64) and macOS 15 (arm64).
   `vm-image` builds both standard and hardened VM disks and their bundle manifests in parallel. Each producer returns its upload's artifact ID.
3. `sign` **waits for the maintainer's approval** on the `release` Environment. It downloads each ID into its own
   directory and checks the filenames and SHA-256 sidecars expected from that producer. It signs and verifies every
   payload with the tagged signing script, then uploads those same files, checksums and signatures together.
4. `release` downloads that upload by ID, attaches its contents to the tag's release and marks it latest.

If a build fails or signing is declined, publication is skipped for every platform, including the web client.
A manual run on a branch produces a workflow artifact for the web build and optionally the VM build.

Each desktop target produces three release assets:

```text
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz.sig
atomics-<version>-aarch64-apple-darwin.tar.gz
atomics-<version>-aarch64-apple-darwin.tar.gz.sha256
atomics-<version>-aarch64-apple-darwin.tar.gz.sig
```

Each VM flavor produces these assets, each with a `.sha256` and `.sig` sidecar:

```text
atomics-vm-<version>-aarch64-linux.raw.zst
atomics-vm-<version>-aarch64-linux.bundle.json
atomics-vm-hardened-<version>-aarch64-linux.raw.zst
atomics-vm-hardened-<version>-aarch64-linux.bundle.json
```

The `sign` job requires all four by exact name and checks their hashes before its signing step sees the key.
A missing or damaged hardened asset prevents promotion to latest just as a missing desktop does. Both VM jobs run
under the same release preflight before signing and use the same isolated signing step. The signed template binds the image digest,
release version and flavor; `atomics-vm.json` in its files records the flavor for setup and later updates.

The Linux archive contains `atomics` at its root. The macOS archive contains `ATOMICS.app`,
assembled by `scripts/macos-app.sh`. Linux builds need glibc 2.35 or later and the runtime
libraries corresponding to the dependencies in the workflow. The app bundle keeps the existing
ad-hoc macOS code signature; the release signature below authenticates the download and does
not provide Apple Developer ID signing or notarization.

`scripts/install.sh` is what the README's one line runs. It reads the release keys from `assets/` on `main`, not from the
release, and then the program installs itself with `atomics install`, as a build downloaded by hand can. A build only
updates itself when it finds a key built in; a debug build can also be pointed at another feed and key for a test
(`ATOMICS_TEST_RELEASE_FEED`, `ATOMICS_TEST_RELEASE_KEY`; VM download fixtures use `ATOMICS_TEST_VM_RELEASE_BASE`
and the same test key). No release holds those paths: the desktop job fails if the release binary contains any of
these names, and a test keeps `debug-assertions` out of the release profile.

The installer requires OpenSSL 3 to verify the release signature. If it cannot find a verifier, it stops before
downloading the archive and prints the command to rerun. On macOS, install the verifier with `brew install openssl@3`.
For a checksum-only install, pass `--skip-signature` to the script or use `ATOMICS_SKIP_SIGNATURE=1` on the shell
that runs it. The same rule applies to `--simple`. The installer still requires a matching SHA-256 and warns that
the download's authenticity was not checked.

```sh
curl -fsSL https://raw.githubusercontent.com/xmagit/ATOMICS/main/scripts/install.sh | ATOMICS_SKIP_SIGNATURE=1 sh
```

## The two release keys

Installed copies trust two public keys, built in from `assets/`, and accept a signature from either:

| Key | Private half | Public half |
| --- | --- | --- |
| CI | Only in the `release` Environment's `ATOMICS_RELEASE_SIGNING_KEY` secret | `assets/release-signing-key.pub` |
| Offline | Encrypted, on no machine that builds or runs agents | `assets/release-signing-key-offline.pub` |

Every release signs with the CI key. The offline key is what recovers from losing it: see [Rotation](#rotation). A key is
Ed25519, and each public file is a PEM SubjectPublicKeyInfo: 44 bytes of DER, the 12-byte prefix
`302a300506032b6570032100` and the 32-byte key, for verifiers that take raw keys. The signer rejects a key that does not
match the public file it is checking against. Verification always uses keys already installed or on `main`; a key that
came with a release is no trust anchor.

## Provision the keys once

This takes the maintainer, and runs nowhere else. First the Environment, with the maintainer as required reviewer, for
`v*` tags only. Self-review stays allowed, since the same account pushes the tag:

```sh
echo '{"prevent_self_review": false, "reviewers": [{"type": "User", "id": 45366887}], "deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}}' | gh api -X PUT repos/xmagit/ATOMICS/environments/release --input -
gh api -X POST repos/xmagit/ATOMICS/environments/release/deployment-branch-policies -f name='v*' -f type=tag
gh api repos/xmagit/ATOMICS/environments/release --jq '.protection_rules[] | .type, .reviewers[]?.reviewer.login'
```

The id is the `xmagit` account's (`gh api users/xmagit --jq .id`), and the last line should print `required_reviewers` and
that login. These work in any shell, fish included.

Or by hand: **Settings → Environments → New environment**, name it `release`, tick **Required reviewers** and add yourself,
then under **Deployment branches and tags** choose **Selected branches and tags** and add a tag rule `v*`.

Then the keys, on a trusted machine with OpenSSL 3 (on a Mac, `brew install openssl@3`), from a checkout of `main`:

```sh
scripts/make-release-keys.sh ~/atomics-release-offline.pem
```

It asks for a passphrase for the offline key, puts the CI key straight into the Environment, writes the encrypted offline
key to the file named, and writes both public keys to `assets/`. It refuses to replace anything. Then:

1. Move the offline key off this machine, and keep its passphrase apart from it. If both keys are ever lost, every install
   has to be updated by hand.
2. Commit the two public files through a pull request. A build made before this can never update itself, so this comes
   before the first release that should.

## Approving a release

The run stops at `sign`, and GitHub notifies the reviewer. **Before approving, check that the run's commit is the one you
meant to release, on `main`:** a tag can point anywhere, and its workflow comes from that commit. Approving signs whatever
the builds made, so this is the moment a person vouches for them. Approve under **Review deployments** on the run, or
reject it and nothing is signed.

## Rotation

Installed copies trust what they were built with, so a change reaches them in a release signed with a key they already
have.

**The CI key was lost, or leaked** (these are POSIX shell commands; in fish, run `sh` first):

1. If it leaked, delete it now: `gh secret delete ATOMICS_RELEASE_SIGNING_KEY --env release --repo xmagit/ATOMICS`. A copy
   that has not updated yet still accepts it, which is why step 5 should come soon: the Environment's reviewer gates this
   workflow's use of the key, not a stolen copy.
2. Make a new one, without replacing the offline key:

   ```sh
   umask 077; key_dir=$(mktemp -d)
   openssl genpkey -algorithm ED25519 -out "$key_dir/ci.pem"
   openssl pkey -in "$key_dir/ci.pem" -pubout -out assets/release-signing-key.pub
   gh secret set ATOMICS_RELEASE_SIGNING_KEY --env release --repo xmagit/ATOMICS < "$key_dir/ci.pem"
   rm -rf "$key_dir"
   ```

3. Commit the new public file, bump the version, and tag it. Reject the `sign` step: installed copies do not trust the new
   key yet.
4. On the trusted machine, download each build artifact by the ID printed by its upload step and extract it into
   `release-builds/<artifact-name>/`. Include `atomics-web`, both desktop targets and the VM build. From the tagged
   checkout, set `version` to the release version without its leading `v` and `offline_key` to the offline PEM path.
   Stage the files and sign them together.

   ```sh
   (
     set -e
     python3 scripts/stage-release.py "$version" release-builds signed
     key=$(openssl pkey -in "$offline_key")
     for checksum in signed/*.sha256; do
       ATOMICS_RELEASE_SIGNING_KEY="$key" sh scripts/sign-release.sh "${checksum%.sha256}" assets/release-signing-key-offline.pub
     done
   )
   ```

   After every signature verifies, create the prerelease with `gh release create v<version> --verify-tag --prerelease`.
   Attach the files with `gh release upload v<version> signed/*`, then run
   `gh release edit v<version> --prerelease=false --latest`.
5. Every install that takes that release trusts the new CI key, and no longer the old one. Later releases sign in CI again.

**The offline key was lost or leaked:** make a new one the same way, encrypted (`-aes-256-cbc`) and kept off this machine,
commit its public file, and release normally: the CI key signs what brings it.

## Verify and test a release

The `.sha256` file is the standard SHA-256 digest, two spaces and the archive basename.
The `.sig` is a **raw 64-byte Ed25519 signature over the exact asset bytes**, with no prehash,
base64 or signature wrapper. Verify both before extracting, decompressing or installing an asset:

```sh
version=0.1.7
target=x86_64-unknown-linux-gnu # aarch64-apple-darwin on Apple Silicon
archive="atomics-$version-$target.tar.gz"
gh release download "v$version" --pattern "$archive*" --dir /tmp/atomics-release-check
cd /tmp/atomics-release-check
shasum -a 256 -c "$archive.sha256"
openssl pkeyutl -verify -rawin -pubin -inkey /trusted/checkout/assets/release-signing-key.pub \
  -in "$archive" -sigfile "$archive.sig"
tar -tzf "$archive"
```

A release signed by hand verifies against `release-signing-key-offline.pub` instead.

Check both VM flavors before booting either image:

```sh
for prefix in atomics-vm atomics-vm-hardened; do
  for suffix in raw.zst bundle.json; do
    asset="$prefix-$version-aarch64-linux.$suffix"
    gh release download "v$version" --pattern "$asset*" --dir /tmp/atomics-release-check
    shasum -a 256 -c "$asset.sha256"
    openssl pkeyutl -verify -rawin -pubin -inkey /trusted/checkout/assets/release-signing-key.pub \
      -in "$asset" -sigfile "$asset.sig"
  done
done
```

Inspect each template's `releaseVersion`, `image`, `imageSha256` and the embedded `atomics-vm.json`. The hardened
template and bundle must say `flavor = hardened` (JSON strings), and its bundle must name an admin and the guest host.
The standard template omits flavor for compatibility with earlier clients; its bundle says `standard`.
Setup verifies these bindings itself and never substitutes a standard image for a missing hardened image.

Before tagging, check YAML with PyYAML installed:

```sh
python3 -c 'import yaml,sys; yaml.safe_load(open(sys.argv[1]))' .github/workflows/web.yml
sh -n scripts/sign-release.sh
sh -n scripts/macos-app.sh
sh -n scripts/install.sh
python3 tests/test_release_signing.py
python3 tests/test_vm_release.py
python3 tests/test_simple_install.py
bash -n scripts/release-vm-image.sh
```

Hosted builds and uploads cannot be proved by a local YAML check. After merging and provisioning the keys, the release
chief sets the intended version in `Cargo.toml` and `Cargo.lock`, then runs `git tag v0.1.7` and `git push origin v0.1.7`
for the planned release. The maintainer approves `sign`. Watch it with `gh run watch <run-id> --exit-status`, and run the
download and check commands above for **both desktop targets and both VM flavors**. Confirm that each archive has the documented contents, that the
web assets are still there, and that `gh release view` no longer calls it a prerelease.

A signed build only proves itself by updating to the next one, so a first release needs a small second one soon after, on
both platforms. To see a missing key's error without touching the real one, use a disposable fork with a matching version
tag, two public files and no secret. The web build should succeed, signing should fail with the setup message, and
the release job should be skipped.
