# Mac signing and notarization: owner checklist

Goal: releases signed with a Developer ID Application certificate and notarized by Apple. Until all steps are done,
builds stay ad-hoc signed and the release text says so. Type secrets into GitHub only, never into chat.
Apple's web and app screens change; steps marked (unverified) are from memory.

## A. Certificate (once)

1. On a Mac, open Keychain Access > Certificate Assistant > Request a Certificate From a Certificate Authority.
   Enter your email, choose "Saved to disk". (unverified UI)
2. Go to developer.apple.com > Account > Certificates, IDs & Profiles > Certificates > "+".
   Choose **Developer ID Application**. Upload the CSR. Download the `.cer` and double-click it to install. (unverified UI)
3. In Keychain Access > My Certificates, find "Developer ID Application: ...". Right-click > Export. Format `.p12`.
   Set an export password and remember it.
4. Encode it: `base64 -i cert.p12 | pbcopy` (the text is now on the clipboard).
5. Delete `cert.p12` from Downloads when finished.

## B. Notarization login (pick one)

6. **Preferred: App Store Connect API key.** App Store Connect > Users and Access > Integrations > Keys > "+"
   (Developer access is enough). Download the `.p8` once. Note the **Key ID** and the **Issuer ID** at the top of the
   page. Encode: `base64 -i AuthKey_XXXX.p8 | pbcopy`. (unverified UI)
7. **Or: Apple ID + app-specific password.** appleid.apple.com > Sign-In and Security > App-Specific Passwords >
   generate one named "aiTrack". (unverified UI)
8. **Team ID:** developer.apple.com > Account > Membership details > Team ID (10 characters). Needed for option 7.

## C. GitHub secrets

9. Repo genen/aiTrack > Settings > Secrets and variables > Actions > New repository secret. Add each by exact name:

| Secret name | Value |
|---|---|
| `APPLE_CERT_P12_BASE64` | base64 of the .p12 (step 4) |
| `APPLE_CERT_PASSWORD` | the .p12 export password |
| `APPLE_API_KEY_P8_BASE64` | base64 of the .p8 (step 6) |
| `APPLE_API_KEY_ID` | Key ID |
| `APPLE_API_ISSUER_ID` | Issuer ID |

Or, instead of the three API key rows:

| Secret name | Value |
|---|---|
| `APPLE_ID` | your Apple ID email |
| `APPLE_APP_PASSWORD` | the app-specific password |
| `APPLE_TEAM_ID` | Team ID |

If both sets exist, the API key is used. With only the certificate, the build is signed but not notarized.

10. Run the macOS workflow by hand (Actions > Build and release macOS app > Run workflow, tag blank). The log step
    "Signing mode" says which mode ran. Pull requests never get the secrets and stay ad-hoc.
11. On a Mac, check a downloaded dmg: `spctl -a -t open --context context:primary-signature -vv aiTrack-*.dmg` and
    `xcrun stapler validate aiTrack-*.dmg`. This has never been run; tell Claude the output if it fails.

## Notes

- If a secret leaks, revoke the certificate or key in the Apple portal and replace the secret.
- The app needs no entitlements (reasons in the header of `scripts/sign-notarize-mac.sh`); unverified on a Mac.
