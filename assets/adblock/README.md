# Bundled ad-filter lists

Pinned snapshot; there is no build-time or runtime list updater (Phase 3).
`src/browser/security/adblock.rs` embeds these files with `include_str!`.

| File | Source | Snapshot | SHA-256 | License |
|---|---|---|---|---|
| `easylist.txt` | https://easylist.to/easylist/easylist.txt | 2026-11-28 (EasyList `202611280450`) | `3de6bbdd38fffc902cd8a2cf06cc95fab41adfee3109ebc1b12fe607fc19ef66` | GPLv3 / CC BY-SA 3.0 |
| `easyprivacy.txt` | https://easylist.to/easylist/easyprivacy.txt | 2026-11-28 (EasyPrivacy `202611280450`) | `350539bee2b18242c29f687b2783e02f925cf2502295422167534c866a91e5e8` | GPLv3 / CC BY-SA 3.0 |
| `ublock-filters.txt` | https://raw.githubusercontent.com/uBlockOrigin/uAssets/edcbb632f6aaec3cfe66519635a1e07a77aadeec/filters/filters.txt | uAssets commit `edcbb63` (2026-11-28) | `f274face037066baf361f7c18215bfcd05f6cd4d040c7c37c6ee90e4d1a1b3fe` | GPLv3 |

The URLhaus hostfile endpoint returned HTTP 410 on 2026-11-28, so no
phishing/malware host list is bundled in this snapshot. The emergency
denylist (`src/browser/security/denylist.rs`) is independent of these lists.

Only network rules are parsed (`RuleTypes::NetworkOnly`); cosmetic filtering
is Phase 3. Filter lists are data, fetched only from their published HTTPS
URLs when preparing a snapshot; no browsing data leaves the machine.
