# Copy error / warning messages (macOS app)

**Date:** 2026-06-30
**Status:** Designed — pending implementation
**Area:** `macos/` SwiftUI app ("Rossum Local") — error presentation
**Builds on:** Phase 2b UI + the Tahoe modernization (merged to `main`)

## Problem

The app surfaces error/warning text in two places, neither of which can be copied:

1. **Operation errors** (`store.lastError` — add / edit / remove / open-existing failures) are
   shown in a modal SwiftUI `.alert` (`ContentView.swift`). A system alert's message is **not
   selectable** and cannot host a context menu, so the text can't be copied.
2. **Sync errors** (`SyncCoordinator.phases[id] == .error(message)`) are shown inline in
   `DetailView` as a `Label(message, …)` — also not selectable.

A user who hits an error (e.g. a sync failure or a bad API base) can't copy the message to paste
into a bug report, Slack, or a search.

## Goal

Let the user copy both error/warning messages, using **selection + a right-click "Copy"** (no
copy buttons). Because a system alert can't be made selectable, the operation-error alert is
replaced with a **dismissible, selectable inline banner**. No change to error *sources* or any
other behavior.

## Non-goals

- No copy *buttons* (per the chosen UX — copy is via selection / context menu; a banner dismiss
  `×` is a dismiss control, not a copy affordance).
- No change to what triggers errors, the model layer, the FFI, or sync behavior.
- No new error categories; only the presentation of the existing `store.lastError` and the
  existing sync `.error` phase.

## Decisions

1. **Copy mechanism:** `.textSelection(.enabled)` + a `.contextMenu { Button("Copy") }` on each
   message. No copy buttons.
2. **Operation errors:** replace the modal `.alert` with a dismissible inline **error banner**
   (selectable text + context-menu copy + a dismiss `×`).
3. **Sync error:** keep the inline `DetailView` `Label`, add selection + context-menu copy.
4. **Clipboard:** a single `Pasteboard.copy(_:)` helper over `NSPasteboard.general`.

## Components

- **`macos/RossumLocal/Support/Pasteboard.swift`** (new)
  ```swift
  import AppKit
  enum Pasteboard {
      @MainActor static func copy(_ string: String) {
          NSPasteboard.general.clearContents()
          NSPasteboard.general.setString(string, forType: .string)
      }
  }
  ```
  Unit-testable: a non-sandboxed XCTest copies a string and reads it back via
  `NSPasteboard.general.string(forType: .string)`.

- **`macos/RossumLocal/Views/ErrorBanner.swift`** (new) — a small view:
  `ErrorBanner(message: String, onDismiss: () -> Void)`. Layout: a `⚠`
  (`exclamationmark.triangle.fill`, orange) + `Text(message)` with `.textSelection(.enabled)`
  and `.contextMenu { Button("Copy") { Pasteboard.copy(message) } }`, a `Spacer`, and a dismiss
  control (`Image(systemName: "xmark")` as a plain `Button` calling `onDismiss`). Tahoe-styled
  background (a `.regularMaterial` rounded rectangle with padding); the whole banner reads as a
  single inset strip.

- **`macos/RossumLocal/Views/ContentView.swift`** (modify) — remove the `.alert("Error", …)`
  block; present the banner at the top of the window content when `store.lastError != nil`, via
  `.safeAreaInset(edge: .top)` on the `NavigationSplitView` (fall back to an
  `.overlay(alignment: .top)` if the inset renders poorly — confirmed on the maintainer relaunch).
  The banner's `onDismiss` sets `store.lastError = nil`.

- **`macos/RossumLocal/Views/DetailView.swift`** (modify) — on the `syncStatus` `.error(let m)`
  `Label`, add `.textSelection(.enabled)` and `.contextMenu { Button("Copy") { Pasteboard.copy(m) } }`.
  The other `syncStatus` arms are unchanged; the switch stays exhaustive.

- **`macos/RossumLocalTests/PasteboardTests.swift`** (new) — `Pasteboard.copy("…")` then assert
  `NSPasteboard.general.string(forType: .string)` equals it (the test target is non-sandboxed, so
  pasteboard access works).

## Verification

- **Compile + unit (CLI / subagent):** `xcodebuild build` succeeds; `xcodebuild test` is now 17
  (16 existing + the pasteboard round-trip), all green.
- **Maintainer (relaunch):** the operation-error banner appears + dismisses, its text selects and
  right-click→Copy works; the DetailView sync error selects and copies. The rendered banner look
  is confirmed visually.

## Backward compatibility

Error *sources* and the model layer are unchanged; only `store.lastError`'s presentation moves
from a modal alert to an inline banner. No on-disk / FFI / CLI-interop impact. macOS 26 target
unchanged.

## Risks / open questions

- The exact banner placement (`.safeAreaInset(edge: .top)` vs `.overlay(alignment: .top)`) renders
  differently across NavigationSplitView layouts; the implementer picks whichever compiles and
  reads cleanly, and the maintainer confirms on relaunch.
- Pasteboard/selection/context-menu behavior at runtime is maintainer-verified; only the
  `Pasteboard.copy` round-trip is unit-tested.
