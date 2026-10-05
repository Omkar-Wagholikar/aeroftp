# v4.2.3 profile preferences: Requested 31

Verified 2026-10-05 against baseline `a22a9c138`, for [tracker #1081](https://github.com/axpdev-lab/aeroftp/issues/1081) and [the reporter's request](https://github.com/axpdev-lab/aeroftp/discussions/347#discussioncomment-18747380).

## Findings and contract

The initial reverse triage inferred that the CLI already followed GUI columns and sort. Live testing disproved that: the CLI read `aeroftp_settings` directly from the vault while the GUI's secure-storage helper writes `config_app_settings`. The CLI also looked up `savedPct` rather than the GUI column id `savedpct`. These read bugs are fixed together with the missing preference writes.

Both surfaces now use `config_app_settings.ui_settings.my_servers_table.visibility` and `ui_settings.my_servers_breakdown`. Explicit CLI `--hide` is subtractive; `--show` is exclusive and takes precedence if both are supplied. Index and Name remain pinned. GUI-only columns, CLI-only columns, table order, widths, alignment, saved sort and unrelated application settings survive preference updates. Missing canonical settings retain the old vault-key fallback; malformed settings or invalid column arguments fail an explicit CLI write without replacing the saved preferences.

`--breakdown` saves true; `--breakdown=false` saves false. The GUI table footer exposes the same setting through the existing translated "Storage by protocol" label. `--sort` remains a one-run override. A saved breakdown affects text output, but bare `profiles --json` remains the existing array; only explicit `--breakdown` requests the JSON object wrapper.

The live cold-start test exposed another gap: before vault unlock the GUI can hydrate from an old browser cache. Both preference hooks now re-read on the existing vault-initialization/unlock refresh as well as window focus.

## Live checks

The isolated portable development vault contained two test profiles. The CLI was built from this branch; the GUI used the unchanged baseline Rust GUI binary with this branch's Vite frontend. WebKitGTK was driven with the `gui-drive` skill, under `G_SLICE=always-malloc`, `G_DEBUG=gc-friendly` and `MALLOC_CHECK_=3`. Tests changed local view preferences only.

| Check | Result |
|---|---|
| GUI hides Host, enables Saved %, sets descending Profile sort and enables protocol breakdown; baseline CLI reads them | Reproduced wrong-key bug: baseline ignores the GUI choices |
| Same GUI choices, new CLI without view flags | Host hidden, Saved % shown, descending GUI sort honored, protocol breakdown present |
| CLI `--show=* --breakdown=false`, then another CLI process | All CLI columns shown; breakdown absent; GUI layout retained |
| Running GUI regains focus after that CLI write | Host and Saved % shown, checkbox off, no protocol rows |
| CLI `--show=name,used --breakdown=false`, then GUI focus | Only the requested data columns shown; GUI Icon and Actions remain available |
| GUI enables Total and protocol breakdown after that exclusive CLI choice, then a fresh CLI process | Used and Total shown, Host remains hidden, breakdown present |
| Saved breakdown true, bare JSON versus explicit JSON breakdown | Bare output remains a two-profile array; explicit output contains profiles, summary and two protocol groups, with identical profile data |
| Invalid `--hide` together with a breakdown change | Exit 5; saved settings unchanged |
| Close GUI with stale browser choices, change CLI preferences, reopen with locked vault and unlock | Correct columns and checkbox loaded immediately after unlock, without a manual focus event |

The GUI exited normally between cold-start runs without a heap-hardening abort. This verification covers profile preferences; it does not claim a new GUI backend build or packaging test.

## Focused gates

- TypeScript typecheck and 16 frontend tests passed (table sanitization, preference hydration, focus/unlock refresh, checkbox writes, preservation and failed writes).
- Five new CLI preference tests and the existing interactive-refresh argument test passed.
- The debug CLI build, Rust formatting, whitespace checks and all five security regression checks passed.
- No locale keys were added or changed; the checkbox reuses an existing translation in all 47 locales.

The PR's complete GitHub Actions matrix remains the authoritative full gate. A green local focused check does not establish the PR matrix as green.
