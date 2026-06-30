# Tahoe (Liquid Glass) modernization of the macOS app

**Date:** 2026-06-30
**Status:** Designed — pending implementation
**Area:** `macos/` SwiftUI app ("Rossum Local") — visual modernization
**Builds on:** Phase 2b (full SwiftUI UI), merged to `main`

## Problem

The macOS app is built with the **macOS 26.5 SDK** (Xcode 26.6) and runs on **macOS 26.3
(Tahoe)**, so standard SwiftUI controls already receive Liquid Glass *automatic restyling*
on Tahoe. But the app still targets macOS 15 and makes no *explicit* use of the Tahoe
design language — primary actions are plain bordered buttons, there is no glass treatment
on key actions, no scroll-edge/background-extension effects, and the app icon is the old
Tauri PNG. We want the app to look intentionally native to Tahoe.

### Grounded API facts (verified against the installed SDK, not assumed)

A search of `MacOSX26.5.sdk/…/SwiftUI.swiftmodule/*.swiftinterface` established what is
actually callable on this toolchain:

- **Available:** `.buttonStyle(.glass)` (`GlassButtonStyle`), `.buttonStyle(.glassProminent)`
  (`GlassProminentButtonStyle`), `backgroundExtensionEffect`, `scrollEdgeEffect`,
  `tabBarMinimizeBehavior`.
- **NOT present in this SDK:** `glassEffect()`, `GlassEffectContainer`, `glassEffectID`,
  `GlassBackgroundEffect` (zero occurrences). The design therefore must **not** depend on
  custom glass-surface APIs — they would not compile here.

These glass APIs are macOS-26-only.

## Goal

A purely-visual modernization of the existing SwiftUI views to Tahoe / Liquid Glass
conventions, using only the SDK-verified APIs, with the deployment target raised to macOS
26 so they can be called directly. Behavior, the model layer, the FFI, and the on-disk
contract are unchanged.

## Non-goals

- **No behavior change.** No new features, no changed flows, no model-layer edits. This is
  styling only.
- **No custom glass surfaces** (`glassEffect`/`GlassEffectContainer`) — not in this SDK.
- **No Swift-6 migration.** `SWIFT_VERSION` stays `5.0`; the Swift-6 work remains a separate
  deferred follow-up (language mode is independent of the deployment target).
- **No hand-authored Liquid Glass app icon.** A real layered icon is an Icon Composer task;
  this ships a simple programmatic placeholder.
- **No backward compatibility with macOS 15–25.** The deployment target is raised to 26
  (the app is unreleased; the maintainer is on Tahoe).

## Decisions

1. **Deployment target:** macOS `15.0` → `26.0` (project options + app target + test target).
   Glass APIs called directly, no `if #available` gates.
2. **Scope:** targeted polish — the SDK-verified glass APIs + Tahoe spacing/typography/SF
   Symbols + a toolbar/sidebar tidy, layered on top of the automatic restyling.
3. **`SWIFT_VERSION`** stays `5.0`.
4. **App icon:** programmatic placeholder (brand-tinted rounded square + glyph), wired as the
   asset-catalog `AppIcon`. Stopgap.
5. **Identity unchanged:** `ai.rossum.local`, "Rossum Local".

## Project changes (`macos/project.yml`)

- `deploymentTarget: macOS: "26.0"` at project options; `deploymentTarget: "26.0"` on the
  `RossumLocal` and `RossumLocalTests` targets.
- Add `macos/RossumLocal/Assets.xcassets` with an `AppIcon` app-icon set, and
  `ASSETCATALOG_COMPILER_APPICON_NAME: AppIcon` on the app target.
- No other manifest changes (SWIFT_VERSION 5.0, bundle id, entitlements, schemes all stay).

## Per-view treatments

All treatments are additive modifiers / restyling on the existing views; the view structure,
bindings, and the model calls are unchanged.

| View | Tahoe treatment (verified APIs only) |
|---|---|
| `DetailView` | **Sync** → `.buttonStyle(.glassProminent)`; Edit/Reveal → `.buttonStyle(.glass)`; Remove/Detach stays destructive (`.glass` + `.tint(.red)` or bordered); `backgroundExtensionEffect()` behind the content; tightened `Grid` spacing + typographic hierarchy; sync status row uses an SF Symbol with a `symbolEffect` (e.g. `.rotate`/`.pulse`) while `.started`. |
| `SidebarView` | `scrollEdgeEffect` on the `List` (content dissolves under the toolbar); refined rows (primary name, secondary api-base, trailing status glyph). Selection + `.contextMenu` unchanged. |
| `AddConnectionSheet` | Add → `.glassProminent`, Cancel → `.glass`; keep `.formStyle(.grouped)`; header + spacing tidy. |
| `EditCredentialsSheet` | Save → `.glassProminent`, Cancel → `.glass`; same form tidy. |
| `EmptyStateView` | larger SF Symbol; refined copy/spacing; **Choose Folder…** → `.glassProminent`. |
| `ContentView` / toolbar | the `+` toolbar item reads correctly in the Tahoe toolbar; `NavigationSplitView` keeps its automatic vibrant sidebar. |

Exact modifier signatures (`scrollEdgeEffect(...)`, `backgroundExtensionEffect()`,
`symbolEffect(...)`) are confirmed against the SDK at implementation time; their *existence*
is already verified above.

## App icon (placeholder)

A small committed render step (Swift/AppKit or Python, whichever is available) produces a
1024×1024 PNG: a rounded square in the product's brand tint with a simple glyph/lettermark.
It is added as the single 1024 image in an `AppIcon` set in `Assets.xcassets`, and the app
target sets `ASSETCATALOG_COMPILER_APPICON_NAME: AppIcon`. This is explicitly a stopgap until
a real Liquid Glass icon is authored in Icon Composer; the render script is kept so it is
reproducible.

## Verification

- **Compile gate (CLI / subagent):** `xcodebuild build` succeeds against macOS 26; the 16
  existing model tests stay green (`xcodebuild test`) — no model code changes, so they must
  not regress.
- **Visual (maintainer):** the controller rebuilds the app ad-hoc-signed and relaunches it;
  the maintainer confirms the rendered Liquid Glass treatments, spacing, and the placeholder
  icon look right. The rendered appearance cannot be verified headlessly.

## Backward compatibility

The app and the `rdc` CLI still share the on-disk contract (unchanged — no model edits). The
only compatibility change is the **deployment-target bump to macOS 26**, which drops macOS
15–25; acceptable because the app is unreleased and the target machine is Tahoe. Bundle id
and product name are unchanged. (Supersedes the macOS-15 floor recorded in the Phase 2
spec.)

## Risks / open questions

- The rendered result is not verifiable headlessly — the maintainer's relaunch is the visual
  gate.
- A `.buttonStyle(.glass)`/`scrollEdgeEffect`/`backgroundExtensionEffect` call could need a
  parameter form different from first guess; existence is verified, exact signatures are
  confirmed at implementation time against the SDK (and caught by the compile gate).
- The placeholder icon is intentionally not polished Liquid Glass art.

## Out of scope / future

A real Liquid Glass app icon (Icon Composer); the Swift-6 migration; any deeper layout rework
(inspector, onboarding redesign, richer status presentation).
