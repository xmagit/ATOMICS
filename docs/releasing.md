# Desktop releases

A `v<version>` tag matching `workspace.package.version` in `Cargo.toml` runs
`.github/workflows/web.yml`:

1. `bundle` and `release` publish the web client, as a **prerelease**. The release that installs and updates read stays
   the one before this, so nobody meets a release with no build for them, however long the next step waits.
2. `desktop` builds on GitHub-hosted Ubuntu 22.04 (x86_64) and macOS 15 (arm64). It sees no key, and fails at once if the
   commit lacks either public key below.
3. `sign` **waits for the maintainer's approval** on the `release` Environment, then signs those builds. It is the only
   job that sees the signing key, and it builds nothing: no cargo, no cache, no dependencies, and the signing script from
   the tagged commit. So nothing a build runs, a `build.rs` or a proc macro, ever comes near the key.
4. `desktop-release` attaches the builds and signatures and makes this the latest release.

A failed build or a signature nobody approves leaves the web client published and the release a prerelease. Manual web
workflow dispatch does not build desktops.

Each desktop target produces three release assets:

```text
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256
atomics-<version>-x86_64-unknown-linux-gnu.tar.gz.sig
atomics-<version>-aarch64-apple-darwin.tar.gz
atomics-<version>-aarch64-apple-darwin.tar.gz.sha256
atomics-<version>-aarch64-apple-darwin.tar.gz.sig
```

The Linux archive contains `atomics` at its root. The macOS archive contains `ATOMICS.app`,
assembled by `scripts/macos-app.sh`. Linux builds need glibc 2.35 or later and the runtime
libraries corresponding to the dependencies in the workflow. The app bundle keeps the existing
ad-hoc macOS code signature; the release signature below authenticates the download and does
not provide Apple Developer ID signing or notarization.

`scripts/install.sh` is what the README's one line runs. It reads the release keys from `assets/` on `main`, not from the
release, and then the program installs itself with `atomics install`, as a build downloaded by hand can. A build only
updates itself when it finds a key built in; a debug build can also be pointed at another feed and key for a test
(`ATOMICS_TEST_RELEASE_FEED`, `ATOMICS_TEST_RELEASE_KEY`). No release holds that path: the desktop job fails if the
release binary contains the name, and a test keeps `debug-assertions` out of the release profile.

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
4. Sign the builds with the offline key, on the trusted machine:

   ```sh
   gh run download <run-id> --pattern 'atomics-unsigned-*' --dir release-builds
   ATOMICS_RELEASE_SIGNING_KEY="$(openssl pkey -in <offline key file>)" \
     sh scripts/sign-release.sh release-builds/<artifact>/<archive> assets/release-signing-key-offline.pub
   ```

   Do that for each archive, then attach the archives with their `.sha256` and `.sig`, and make it the latest:
   `gh release upload v<version> <files>`, then `gh release edit v<version> --prerelease=false --latest`.
5. Every install that takes that release trusts the new CI key, and no longer the old one. Later releases sign in CI again.

**The offline key was lost or leaked:** make a new one the same way, encrypted (`-aes-256-cbc`) and kept off this machine,
commit its public file, and release normally: the CI key signs what brings it.

## Verify and test a release

The `.sha256` file is the standard SHA-256 digest, two spaces and the archive basename.
The `.sig` is a **raw 64-byte Ed25519 signature over the exact tar.gz bytes**, with no prehash,
base64 or signature wrapper. Verify both before extracting or installing an archive:

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

Before tagging, check YAML with PyYAML installed:

```sh
python3 -c 'import yaml,sys; yaml.safe_load(open(sys.argv[1]))' .github/workflows/web.yml
sh -n scripts/sign-release.sh
sh -n scripts/macos-app.sh
sh -n scripts/install.sh
python3 tests/test_release_signing.py
```

Hosted builds and uploads cannot be proved by a local YAML check. After merging and provisioning the keys, the release
chief sets the intended version in `Cargo.toml` and `Cargo.lock`, then runs `git tag v0.1.7` and `git push origin v0.1.7`
for the planned release. The maintainer approves `sign`. Watch it with `gh run watch <run-id> --exit-status`, and run the
download and check commands above for **both** targets. Confirm that each archive has the documented contents, that the
web assets are still there, and that `gh release view` no longer calls it a prerelease.

A signed build only proves itself by updating to the next one, so a first release needs a small second one soon after, on
both platforms. To see a missing key's error without touching the real one, use a disposable fork with a matching version
tag, two public files and no secret: `sign` must fail with the setup message, and `bundle` and `release` succeed.
