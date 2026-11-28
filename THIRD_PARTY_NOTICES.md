# Third-party notices

This file records third-party components and data added for browser security.

## adblock-rust (`adblock` crate)

- Source: https://github.com/brave/adblock-rust
- License: Mozilla Public License 2.0 (MPL-2.0)
- Use: network request filtering in `src/browser/security/adblock.rs`, built
  only with the `cef-engine` feature. Unmodified crate code.

## psl

- Source: https://github.com/addr-rs/psl
- License: MIT OR Apache-2.0
- Use: registrable-domain (eTLD+1) matching for navigation decisions.

## Filter lists

Bundled under `assets/adblock/`; exact sources, dates, SHA-256 values, and
licenses are recorded in `assets/adblock/README.md`.

- EasyList and EasyPrivacy: GPLv3 / CC BY-SA 3.0 (see https://easylist.to/).
- uBlock Origin filters (`uAssets`): GPLv3
  (see https://github.com/uBlockOrigin/uAssets/blob/master/LICENSE).

Filter lists are treated as data and are not modified. No list updater is
included; the checked-in snapshot is the source of truth. Do not bundle
non-commercial lists in a paid build without review.
