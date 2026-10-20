# Making a release

A release is one pushed tag. The [release workflow](../.github/workflows/release.yml) does the rest: it builds the installer on GitHub's own runner, publishes the GitHub release with the installer, its SHA-256 and the changelog entry, and sends the new version to winget.

## Steps

1. Set the version, in all three places at once:
   ```
   node scripts/bump-version.mjs 1.0.1
   cargo check --workspace
   ```
2. Add the entry to `CHANGELOG.md` under a heading `## [1.0.1] - YYYY-MM-DD`. The release notes are taken from it, so the release fails without it.
3. Commit, and push `main`.
4. Tag that commit and push the tag:
   ```
   git tag v1.0.1
   git push origin v1.0.1
   ```
5. Watch the run under Actions › Release. It takes about half an hour. When it is green, the release is on the Releases page and a pull request is open on `microsoft/winget-pkgs`; winget shows the new version once Microsoft's checks have merged it, usually within a day.

The tag must equal the version in the tree (`v1.0.1` for `1.0.1`); the workflow stops at once when it does not. Running the workflow again for the same tag replaces the installer on the existing release. Each release carries the installer twice: under its versioned name (winget uses that one) and as `lintel-setup.exe`, which the website's download button falls back to.

The website is not part of the pipeline: its download card reads the latest release from GitHub by itself, so a release needs no change there.

## One-time setup for winget

The first version of the package was submitted by hand. For the workflow to send later versions, the repository needs one secret:

1. On GitHub, create a classic personal access token with the `public_repo` scope only (Settings › Developer settings › Personal access tokens › Tokens (classic)).
2. Save it as the secret `WINGET_TOKEN` of this repository (Settings › Secrets and variables › Actions), or from a terminal: `gh secret set WINGET_TOKEN --repo shakibbinkabir/lintel` and paste the token when asked.

Without the secret the release still builds and publishes; only the winget step is skipped, with a notice saying so.

## Building without releasing

Actions › Build › Run workflow builds the installer from any branch and keeps it as an artifact of the run, without publishing anything.

## Code signing

Lintel's releases are not code-signed. The SignPath Foundation, which signs open-source projects for free, reviewed Lintel's application in October 2026 and declined it for now, because the project is still young: it looks for public signals such as stars, forks, contributors and outside mentions. Lintel may apply again later. A Microsoft Store version is being prepared; the Store signs the apps it distributes, so that version will be signed by Microsoft.

Until then, every release is built on GitHub's own runner from the tagged source by the release workflow, and the release page lists the installer's SHA-256. Anyone can also build it from source. SmartScreen warns on an unsigned installer: choose "More info", then "Run anyway", if you trust the download.

If a certificate is ever obtained, the signing step goes at the marked place in `.github/workflows/release.yml`, between the build artifact and the publish step.
