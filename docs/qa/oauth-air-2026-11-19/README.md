# OAuth visual refresh — validation

The OAuth login handoff, client consent, login-code and error pages now use the portal's dark background, mint actions, S mark and generous spacing. The stylesheet is embedded in the page and authorized by the existing SHA-256 CSP, so the standalone OAuth router keeps its styling without a portal asset dependency.

## Verified behavior

| Check | Result |
| --- | --- |
| Existing OAuth unit tests | 22 passed, 0 failed |
| Responsive OAuth pages | 36 passed: 9 states at 4 CSS viewports |
| Browser behavior checks | 10 passed |
| JavaScript disabled | 4 pages passed |
| Synthetic injected pricing fragment | 4 widths passed |
| Rust formatting and whitespace | Passed |

The CSS viewports were 1440×1100, 768×1024, 390×844 and 320×740. The responsive checks covered provider choice, unavailable provider, login handoff, client return, error, consent, login-code input, code error and long client/resource values. They checked horizontal overflow, actual dark styling under the served CSP, branding, script errors and network errors.

Browser checks retained POST form actions, hidden transaction IDs, allow/cancel values, password input constraints, provider selection and keyboard accessibility. The existing unit tests cover the actual authorization logic: CSRF, provider state, PKCE, replay protection, callback restrictions, cancellation and script-free IPv6 navigation. The unit-test command was `cargo test --locked --offline --lib oauth::tests -- --test-threads=2`, using the shared development target directory.

The pricing fragment now has its own 1120px maximum width. Its measured left/right margins were 160px at 1440, 48px at 768, 24px at 390 and 20px at 320. This is a synthetic fragment check; production proxy HTML and configuration were not changed.

## Evidence and scope

All screenshots use synthetic identities, client names, callback origins and transaction IDs. The fixture renderer extracts the current Rust `page`, `page_with_head` and `consent_page` functions. Small data structures and a synthetic URL-origin helper supply fixture data; the preview is a layout test, not a substitute for the real OAuth tests.

The handoff screenshots suppress the zero-delay refresh only to make that intermediate screen visible. A separate fixture kept the zero-delay meta refresh and reached a local synthetic callback without JavaScript. No real sign-in, authorization submission, API POST or production deployment occurred.

Two initial browser assertions expected values that were absent from their fixtures. They were corrected to the exact synthetic client name and local callback target and rerun successfully. The final report has no failed checks. The pricing spacing follow-up was verified separately, and all six screenshots were regenerated with the final stylesheet.

| Screenshot | Capture |
| --- | --- |
| [desktop-redirect.png](desktop-redirect.png) | 1440×1100 login handoff |
| [desktop-consent.png](desktop-consent.png) | 1440×1100 consent |
| [mobile-redirect.png](mobile-redirect.png) | 390×844 login handoff |
| [mobile-consent.png](mobile-consent.png) | 390×844 consent |
| [mobile-login-code.png](mobile-login-code.png) | 390px wide full page, 1118px tall; CSS viewport 390×844 |
| [small-mobile-error.png](small-mobile-error.png) | 320×740 error |

The six captures were visually inspected. The machine-readable results, source hashes and screenshot sizes are in [browser-qa.json](browser-qa.json).

## Review notes

The change is limited to presentation and pricing spacing. Existing auth checks, redirect destinations, escaping, form fields and CSP generation remain in the Rust implementation. The browser evidence supports the layout and form checks above; it does not claim a production deployment or live provider login test.
