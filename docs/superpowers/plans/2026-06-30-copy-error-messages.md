# Copy Error / Warning Messages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make both error/warning surfaces copyable via selection + a right-click "Copy" — replacing the un-copyable modal error alert with a dismissible selectable banner, and adding selection/context-menu copy to the DetailView sync error.

**Architecture:** A `Pasteboard.copy` helper over `NSPasteboard`; a new `ErrorBanner` view (selectable text + context-menu Copy + dismiss ×) shown at the top of `ContentView` for `store.lastError`; `.textSelection`+`.contextMenu` added to the DetailView sync `.error` label. No model/behavior change.

**Tech Stack:** SwiftUI + AppKit (`NSPasteboard`), macOS 26, XCTest, `xcodebuild`.

## Global Constraints

- macOS 26 target; `SWIFT_VERSION 5.0`; Swift module `RossumLocal`.
- **No copy buttons** — copy is via `.textSelection(.enabled)` + `.contextMenu { Button("Copy") }`. (A banner dismiss `×` is a dismiss control, not a copy affordance.)
- **No model/behavior change.** Error sources (`store.lastError`, `SyncCoordinator.phases`) are unchanged; only `store.lastError`'s *presentation* moves from a modal `.alert` to an inline banner. The existing model tests must stay green.
- Verified APIs only (`NSPasteboard`, `.textSelection`, `.contextMenu`, `.safeAreaInset`).
- Customer confidentiality: no customer names/identifiers anywhere (code, tests, commit messages). Neutral placeholders only.
- Verification: per-task gate = `xcodebuild build` + `xcodebuild test`, `CODE_SIGNING_ALLOWED=NO`, FOREGROUND, Bash timeout up to 600000. The rendered banner + selection/copy *behavior* is MAINTAINER-verified on the controller's relaunch; only `Pasteboard.copy` is unit-tested.

## File Structure

| File | Change |
|---|---|
| `macos/RossumLocal/Support/Pasteboard.swift` (create) | `Pasteboard.copy(_:)` |
| `macos/RossumLocalTests/PasteboardTests.swift` (create) | round-trip test |
| `macos/RossumLocal/Views/ErrorBanner.swift` (create) | selectable dismissible banner |
| `macos/RossumLocal/Views/ContentView.swift` (modify) | remove `.alert`; show `ErrorBanner` via `.safeAreaInset` |
| `macos/RossumLocal/Views/DetailView.swift` (modify) | sync `.error` label: `.textSelection` + `.contextMenu` Copy |

---

## Task 1: Pasteboard helper + test

**Files:** Create `macos/RossumLocal/Support/Pasteboard.swift`, `macos/RossumLocalTests/PasteboardTests.swift`

**Interfaces:**
- Produces: `enum Pasteboard { @MainActor static func copy(_ string: String) }`.

- [ ] **Step 1: Write the failing test**

`macos/RossumLocalTests/PasteboardTests.swift`:
```swift
import XCTest
import AppKit
@testable import RossumLocal

@MainActor
final class PasteboardTests: XCTestCase {
    func testCopyWritesStringToGeneralPasteboard() {
        let message = "Sync failed: host example.test unreachable"
        Pasteboard.copy(message)
        XCTAssertEqual(NSPasteboard.general.string(forType: .string), message)
    }
}
```

- [ ] **Step 2: Run it — expect a COMPILE failure (red)**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: build/test FAILS — `cannot find 'Pasteboard' in scope`.

- [ ] **Step 3: Create the helper**

`macos/RossumLocal/Support/Pasteboard.swift`:
```swift
import AppKit

/// Writes a string to the general pasteboard. Used by the error banner and the
/// sync-error context menu so users can copy a message to paste elsewhere.
enum Pasteboard {
    @MainActor static func copy(_ string: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(string, forType: .string)
    }
}
```

- [ ] **Step 4: Run it — green**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: `** TEST SUCCEEDED **`, `testCopyWritesStringToGeneralPasteboard` passes; the prior 16 tests stay green (17 total).

- [ ] **Step 5: Commit**

```bash
git add macos/RossumLocal/Support/Pasteboard.swift macos/RossumLocalTests/PasteboardTests.swift
git commit -m "feat(macos): Pasteboard.copy helper + round-trip test

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: ErrorBanner view + replace the error alert

**Files:** Create `macos/RossumLocal/Views/ErrorBanner.swift`; modify `macos/RossumLocal/Views/ContentView.swift`

**Interfaces:**
- Consumes: `Pasteboard.copy`, `store.lastError`.
- Produces: `ErrorBanner(message: String, onDismiss: () -> Void)`.

- [ ] **Step 1: Create the banner**

`macos/RossumLocal/Views/ErrorBanner.swift`:
```swift
import SwiftUI

/// A dismissible, selectable error/warning strip shown at the top of the window.
/// The message text is selectable and offers a right-click "Copy"; the × dismisses.
struct ErrorBanner: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(message)
                .textSelection(.enabled)
                .contextMenu { Button("Copy") { Pasteboard.copy(message) } }
                .frame(maxWidth: .infinity, alignment: .leading)
            Button(action: onDismiss) {
                Image(systemName: "xmark").imageScale(.small)
            }
            .buttonStyle(.plain)
            .help("Dismiss")
        }
        .padding(10)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(.orange.opacity(0.35)))
        .padding(.horizontal, 12)
        .padding(.top, 8)
    }
}
```

- [ ] **Step 2: Replace the `.alert` in `ContentView`**

In `macos/RossumLocal/Views/ContentView.swift`, REMOVE the `.alert("Error", isPresented: …) { Button("OK") … } message: { Text(store.lastError ?? "") }` block, and add this `.safeAreaInset` on the `NavigationSplitView` (alongside the other modifiers):
```swift
        .safeAreaInset(edge: .top) {
            if let err = store.lastError {
                ErrorBanner(message: err) { store.lastError = nil }
            }
        }
```
(When `store.lastError` is nil the inset content is empty, so it reserves no space. If `.safeAreaInset` renders poorly across the split-view columns, switch to `.overlay(alignment: .top) { if let err = store.lastError { ErrorBanner(message: err) { store.lastError = nil } } }` — note which you used in the report.)

- [ ] **Step 3: Build + test**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO build test`
Expected: `** BUILD SUCCEEDED **` then `** TEST SUCCEEDED **` (17). The `.alert` is gone; nothing else references it.

- [ ] **Step 4: Commit**

```bash
git add macos/RossumLocal/Views/ErrorBanner.swift macos/RossumLocal/Views/ContentView.swift
git commit -m "feat(macos): replace error alert with selectable dismissible ErrorBanner

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: DetailView sync-error copy + final build

**Files:** Modify `macos/RossumLocal/Views/DetailView.swift`

**Interfaces:**
- Consumes: `Pasteboard.copy`; the existing `syncStatus` `.error(let m)` arm.

- [ ] **Step 1: Add selection + context-menu copy to the sync error**

In `DetailView.swift`, the `syncStatus` `.error` arm is currently:
```swift
        case .error(let m): Label(m, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
```
Change it to:
```swift
        case .error(let m):
            Label(m, systemImage: "exclamationmark.triangle")
                .foregroundStyle(.orange)
                .textSelection(.enabled)
                .contextMenu { Button("Copy") { Pasteboard.copy(m) } }
```
Leave the `.started`, `.done`, and `.none` arms unchanged; the switch stays exhaustive.

- [ ] **Step 2: Final clean build + test**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO clean build test`
Expected: `** BUILD SUCCEEDED **` then `** TEST SUCCEEDED **` (17 tests, 0 failures).

- [ ] **Step 3: Commit**

```bash
git add macos/RossumLocal/Views/DetailView.swift
git commit -m "feat(macos): make the sync error selectable + right-click copyable

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage** (against `2026-06-30-copy-error-messages-design.md`):
- `Pasteboard.copy` + unit test: Task 1. ✓
- `ErrorBanner` (selectable + context-menu Copy + dismiss): Task 2. ✓
- ContentView alert → banner (`.safeAreaInset`/overlay): Task 2. ✓
- DetailView sync-error selection + context-menu Copy: Task 3. ✓
- No copy buttons / no model change / verified APIs: Global Constraints + per-task. ✓
- 17 tests green: Tasks 1–3 gates. ✓

**Placeholder scan:** No TBD/TODO. The only conditional is Task 2's documented `.safeAreaInset`↔`.overlay` fallback (bounded, maintainer-confirmed). All code is complete.

**Type consistency:** `Pasteboard.copy(_:)` (Task 1) is called identically in `ErrorBanner` (Task 2) and `DetailView` (Task 3); `ErrorBanner(message:onDismiss:)` matches its ContentView call site; `store.lastError` is the existing `var lastError: String?`.

**Verification reality:** the pasteboard helper is genuinely unit-tested; the banner rendering, text selection, and context-menu Copy *behavior* are maintainer-verified on the relaunch (the compile gate + the unit test are the automated coverage).

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-30-copy-error-messages.md`.
