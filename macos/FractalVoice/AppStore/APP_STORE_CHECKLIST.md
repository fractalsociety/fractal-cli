# Fractal Voice Mac App Store checklist

## Prepared in the repository

- App category: Developer Tools
- Microphone purpose string
- Human-readable copyright
- Privacy manifest covering account identity and published project content
- Separate main-app and child-process sandbox entitlement templates
- English product-page copy and review notes

## Architecture work required before submission

- Replace unrestricted `~/.fractal`, `~/fractal-projects`, and `~/Library/Logs`
  access with container storage and security-scoped, user-selected project
  folders.
- Persist selected project-folder access with an app-scoped security-scoped
  bookmark.
- Replace `/usr/bin/curl` subprocess downloads with `URLSession`.
- Validate whether sandboxed Fractal child processes can launch and communicate
  with user-installed Codex, Claude, Cursor, Hermes, and Git. If not, keep the
  full agent workflow in the Developer ID edition and define a sandbox-safe
  App Store feature set.
- Sign bundled child executables with the child sandbox entitlements and the
  main executable with the main sandbox entitlements.
- Create the App Store bundle identifier, Mac App Distribution certificate,
  Mac Installer Distribution certificate, and provisioning profile.

## App Store Connect owner actions

- Create the macOS app record for `com.fractalsociety.voice`.
- Supply SKU, pricing, territories, age rating, content-rights declaration,
  and Digital Services Act trader status.
- Publish and enter working Support and Privacy Policy URLs.
- Complete App Privacy answers consistent with `PrivacyInfo.xcprivacy`.
- Provide one to ten Mac screenshots without transparency.
- Provide App Review contact information, a test account, and review workflow.
- Complete encryption/export-compliance questions.

## Release validation

- Archive using the App Store provisioning profile.
- Validate the archive in Xcode Organizer.
- Test the sandboxed build from a clean macOS user account.
- Upload to App Store Connect and resolve all processing warnings.
- Test with TestFlight for Mac before submitting for review.
