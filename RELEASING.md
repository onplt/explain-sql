# Releasing ExplainSQL

A release publishes:
- prebuilt binaries and install scripts on GitHub Releases;
- the four crates on [crates.io](https://crates.io/crates/explainsql): `explainsql-core`, `explainsql-db`, `explainsql-tui` and the `explainsql` binary, which depends on the other three.

Published crate versions are public and permanent: they can be yanked, never deleted.

## Once: the first release

1. **Make the repository public.** The crates' repository links, the README's demo image and the install scripts need it.
2. **Prepare crates.io.** Log in at [crates.io](https://crates.io) with your GitHub account. In [Account Settings](https://crates.io/settings/profile), add and verify an email address; crates.io refuses to publish without one.
3. **Create a token.** In [API Tokens](https://crates.io/settings/tokens), create a token with the `publish-new` and `publish-update` scopes and a short expiry. Then log in with it on your machine:

   ```sh
   cargo login            # paste the token
   ```

4. **Publish the crates.** On an up-to-date `main` whose CI is green:

   ```sh
   cargo publish --workspace --dry-run --locked   # packages and builds every crate; uploads nothing
   cargo publish --workspace --locked             # publishes them in dependency order
   ```

   Then check that `cargo install explainsql --locked` works on another machine.
5. **Tag the release.** The release workflow builds and tests the binaries and publishes the GitHub release. Its crates.io job skips the versions that are already published.

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

6. **Turn on Trusted Publishing.** For each of the four crates, open its Settings → Trusted Publishing on crates.io and add a GitHub publisher:
   - owner `onplt`;
   - repository `explain-sql`;
   - workflow `release.yml`;
   - environment left empty.

   Then revoke the token from step 3 (and `cargo logout`). From now on, releases need no token.
7. **Publish the documentation site.** Enable it once: Settings → Pages → Source: GitHub Actions. Then run the Docs workflow by hand, or let the next release tag deploy it.

## Every release after that

1. Raise `version` in the root `Cargo.toml` (`[workspace.package]` and the three internal dependencies under `[workspace.dependencies]`). Then run `cargo check` to update `Cargo.lock`.
2. Move the `Unreleased` notes in `CHANGELOG.md` under the new version and date. The release notes are taken from that section.
3. Commit, push to `main`, and wait for CI to pass, including the `crates.io packages` job.
4. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.

The release workflow checks that the tag matches the version. It builds and smoke-tests the binaries for every target, publishes the GitHub release, and publishes the crates through Trusted Publishing.

## If something goes wrong

- **A bad crate version:** `cargo yank --version X.Y.Z explainsql` keeps new projects from picking it, and existing lockfiles keep working. Fix the problem and release a new patch version; a published version cannot be replaced.
- **A failed release workflow:** fix the cause on `main`, delete the tag (`git push --delete origin vX.Y.Z` and `git tag -d vX.Y.Z`) and the draft release if one was created, then tag again. Crate versions that did get published are skipped on the second run.
