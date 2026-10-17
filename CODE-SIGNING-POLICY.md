# Code signing policy

## Windows

Windows releases are planned to be signed with a certificate provided free of charge to open-source projects by the
[SignPath Foundation](https://signpath.org), using the [SignPath.io](https://signpath.io) service. **Until that
application is approved, Windows downloads are not signed**, and Windows SmartScreen may show "Unknown publisher"
(choose "Run anyway"). This file will be updated when signing starts.

## macOS

The macOS app is signed with an Apple Developer ID and notarized by Apple.

## How a signed build is made

- The build runs on GitHub Actions from the public source in this repository
  (`.github/workflows/release-msi.yml` for Windows, `.github/workflows/release-macos.yml` for macOS). Nothing built
  on a developer's own computer is published.
- A release is started by hand by the project owner with a version tag. Signing happens only in that run.
- The signing keys and passwords are kept in GitHub Actions secrets and are never printed or stored in the repository.

## Roles

- **Committers, reviewers and approvers:** the project owner (GitHub: [genen](https://github.com/genen)). Every
  release is approved by the project owner before it is started.

## Privacy

See [PRIVACY.md](PRIVACY.md): aiTrack does not transfer any information to other networked systems except the three
requests listed there, which are needed for it to work.

## Reporting a problem

If you think a signed file is wrong or malicious, write to aitrack@sirgene.com.
