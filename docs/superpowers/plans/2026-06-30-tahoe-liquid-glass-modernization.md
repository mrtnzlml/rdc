# Tahoe / Liquid Glass Modernization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Modernize the existing "Rossum Local" SwiftUI views to macOS Tahoe / Liquid Glass conventions using only SDK-verified APIs, raising the deployment target to macOS 26, with no behavior change.

**Architecture:** Pure-visual restyling of the 6 existing views layered on the automatic Liquid Glass restyling the macOS-26 SDK already applies. Primary actions adopt `.buttonStyle(.glassProminent)`/`.glass`; the sidebar list adopts `.scrollEdgeEffectStyle`; Tahoe spacing/typography/SF-Symbol refinements throughout. A programmatic placeholder app icon replaces the old Tauri PNG. The model layer, FFI, and on-disk contract are untouched.

**Tech Stack:** SwiftUI (macOS 26), the verified Liquid Glass APIs, AppKit (icon render), XcodeGen, `xcodebuild`.

## Global Constraints

- **Deployment target:** macOS `26.0` (raised from `15.0`) on project options + app target + test target. Glass APIs called directly, no `if #available` gates.
- **`SWIFT_VERSION` stays `5.0`.** No Swift-6 migration here.
- **Bundle id** `ai.rossum.local`, **product name** `Rossum Local` — unchanged.
- **No behavior change / no model edits.** Styling only. The 16 existing tests must stay green and are the regression guard.
- **Verified-available APIs ONLY** (confirmed against `MacOSX26.5.sdk` SwiftUI interface): `.buttonStyle(.glass)`, `.buttonStyle(.glassProminent)`, `.scrollEdgeEffectStyle(_:for:)` (style `.soft`/`.hard`), `.backgroundExtensionEffect()`, `.symbolEffect(_:options:isActive:)`. **Do NOT use** `glassEffect()`/`GlassEffectContainer`/`glassEffectID` — not in this SDK.
- **Customer confidentiality:** no customer names/identifiers anywhere (code, scripts, docs, commit messages). Neutral placeholders only.
- **Verification:** per-task gate = `xcodebuild build` (compile, macOS 26) + `xcodebuild test` (16 green), `CODE_SIGNING_ALLOWED=NO`, FOREGROUND, Bash timeout up to 600000. The rendered visual result is MAINTAINER-verified (the controller rebuilds ad-hoc + relaunches; only the maintainer can confirm the look).

## File Structure

| File | Change |
|---|---|
| `macos/project.yml` | deploymentTarget 15→26 (3 places); add `ASSETCATALOG_COMPILER_APPICON_NAME: AppIcon` |
| `macos/tools/make-app-icon.swift` (create) | AppKit renderer → 1024 master PNG |
| `macos/tools/build-app-icon.sh` (create) | run renderer + `sips` → fill `AppIcon.appiconset` + Contents.json |
| `macos/RossumLocal/Assets.xcassets/AppIcon.appiconset/*` (create) | generated icon set (committed) |
| `macos/RossumLocal/Views/DetailView.swift` | glass button styles; spacing/typography; sync `symbolEffect` |
| `macos/RossumLocal/Views/SidebarView.swift` | `scrollEdgeEffectStyle`; refined rows |
| `macos/RossumLocal/Views/AddConnectionSheet.swift` | glass buttons |
| `macos/RossumLocal/Views/EditCredentialsSheet.swift` | glass buttons |
| `macos/RossumLocal/Views/EmptyStateView.swift` | glassProminent CTA; bigger symbol |
| `macos/README.md` | Tahoe/macOS-26 note |

---

## Task 1: Raise deployment target to macOS 26

**Files:** Modify `macos/project.yml`

**Interfaces:**
- Produces: an app + test target deploying to macOS 26.0 that still builds and passes the 16 tests (baseline before any styling).

- [ ] **Step 1: Edit `macos/project.yml` deployment targets**

Change the three deployment-target occurrences from `"15.0"` to `"26.0"`:
- project `options.deploymentTarget.macOS`
- `targets.RossumLocal.deploymentTarget`
- `targets.RossumLocalTests.deploymentTarget`

- [ ] **Step 2: Regenerate + build + test**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO build test`
Expected: `** BUILD SUCCEEDED **` then `** TEST SUCCEEDED **` (16 tests). No code changed, so this just confirms the target bump is clean.

- [ ] **Step 3: Commit**

```bash
git add macos/project.yml
git commit -m "build(macos): raise deployment target to macOS 26 (Tahoe)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Placeholder app icon

**Files:** Create `macos/tools/make-app-icon.swift`, `macos/tools/build-app-icon.sh`, `macos/RossumLocal/Assets.xcassets/AppIcon.appiconset/`; modify `macos/project.yml`

**Interfaces:**
- Produces: an `AppIcon` asset-catalog set wired into the app; the build embeds it.

> The icon is a deliberate placeholder. The render script is committed so it's reproducible; a real Liquid Glass icon is a future Icon Composer task.

- [ ] **Step 1: Create the renderer `macos/tools/make-app-icon.swift`**

```swift
import AppKit

// Renders a 1024x1024 placeholder app icon: a tinted rounded square with a
// white SF Symbol glyph centered. Output path is argv[1].
let size = 1024.0
let outPath = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon-1024.png"

let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()
let rect = NSRect(x: 0, y: 0, width: size, height: size)

// Background: vertical gradient in a neutral indigo/blue (placeholder brand tint).
let top = NSColor(srgbRed: 0.20, green: 0.42, blue: 0.85, alpha: 1)
let bottom = NSColor(srgbRed: 0.12, green: 0.26, blue: 0.60, alpha: 1)
let radius = size * 0.2237   // squircle-ish; the system masks to its own shape anyway
let path = NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius)
path.addClip()
NSGradient(starting: top, ending: bottom)?.draw(in: rect, angle: -90)

// Glyph: a white SF Symbol centered at ~52% of the canvas.
let cfg = NSImage.SymbolConfiguration(pointSize: size * 0.52, weight: .semibold)
if let sym = NSImage(systemSymbolName: "tray.and.arrow.down.fill", accessibilityDescription: nil)?
    .withSymbolConfiguration(cfg) {
    let tinted = NSImage(size: sym.size)
    tinted.lockFocus()
    NSColor.white.set()
    let r = NSRect(origin: .zero, size: sym.size)
    sym.draw(in: r)
    r.fill(using: .sourceAtop)
    tinted.unlockFocus()
    let gx = (size - sym.size.width) / 2
    let gy = (size - sym.size.height) / 2
    tinted.draw(in: NSRect(x: gx, y: gy, width: sym.size.width, height: sym.size.height))
}
image.unlockFocus()

guard let tiff = image.tiffRepresentation,
      let rep = NSBitmapImageRep(data: tiff),
      let png = rep.representation(using: .png, properties: [:]) else {
    FileHandle.standardError.write("failed to render icon\n".data(using: .utf8)!)
    exit(1)
}
try! png.write(to: URL(fileURLWithPath: outPath))
print("wrote \(outPath)")
```

- [ ] **Step 2: Create `macos/tools/build-app-icon.sh`**

```bash
#!/usr/bin/env bash
# Render the placeholder master icon and fill the macOS AppIcon.appiconset.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SET="$HERE/../RossumLocal/Assets.xcassets/AppIcon.appiconset"
mkdir -p "$SET"
MASTER="$HERE/icon-1024.png"

swift "$HERE/make-app-icon.swift" "$MASTER"

# macOS app-icon sizes: (px, filename)
emit() { sips -z "$1" "$1" "$MASTER" --out "$SET/$2" >/dev/null; }
emit 16   icon_16.png
emit 32   icon_16@2x.png
emit 32   icon_32.png
emit 64   icon_32@2x.png
emit 128  icon_128.png
emit 256  icon_128@2x.png
emit 256  icon_256.png
emit 512  icon_256@2x.png
emit 512  icon_512.png
cp "$MASTER" "$SET/icon_512@2x.png"   # 1024

cat > "$SET/Contents.json" <<'JSON'
{
  "images" : [
    { "size":"16x16","idiom":"mac","filename":"icon_16.png","scale":"1x" },
    { "size":"16x16","idiom":"mac","filename":"icon_16@2x.png","scale":"2x" },
    { "size":"32x32","idiom":"mac","filename":"icon_32.png","scale":"1x" },
    { "size":"32x32","idiom":"mac","filename":"icon_32@2x.png","scale":"2x" },
    { "size":"128x128","idiom":"mac","filename":"icon_128.png","scale":"1x" },
    { "size":"128x128","idiom":"mac","filename":"icon_128@2x.png","scale":"2x" },
    { "size":"256x256","idiom":"mac","filename":"icon_256.png","scale":"1x" },
    { "size":"256x256","idiom":"mac","filename":"icon_256@2x.png","scale":"2x" },
    { "size":"512x512","idiom":"mac","filename":"icon_512.png","scale":"1x" },
    { "size":"512x512","idiom":"mac","filename":"icon_512@2x.png","scale":"2x" }
  ],
  "info" : { "author":"xcode","version":1 }
}
JSON
echo "AppIcon.appiconset written to $SET"
```

- [ ] **Step 3: Run it**

Run: `chmod +x macos/tools/build-app-icon.sh && macos/tools/build-app-icon.sh`
Expected: `AppIcon.appiconset written…`; the set contains 10 PNGs + Contents.json. (`swift` and `sips` ship with macOS/Xcode.)

- [ ] **Step 4: Wire the icon in `macos/project.yml`**

Add to the `RossumLocal` target's `settings.base`:
```yaml
        ASSETCATALOG_COMPILER_APPICON_NAME: AppIcon
```

- [ ] **Step 5: Regenerate + build**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO build`
Expected: `** BUILD SUCCEEDED **` with no asset-catalog errors (the `AppIcon` set compiles).

- [ ] **Step 6: Commit**

```bash
git add macos/tools macos/RossumLocal/Assets.xcassets macos/project.yml
git commit -m "feat(macos): placeholder app icon (programmatic) wired as AppIcon

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Glass button styles on primary/secondary actions

**Files:** Modify `DetailView.swift`, `AddConnectionSheet.swift`, `EditCredentialsSheet.swift`, `EmptyStateView.swift`

**Interfaces:**
- Consumes: the existing buttons in those views (read each file; apply the modifiers below to the named buttons).
- Produces: Tahoe glass styling on actions; no logic change.

The glass button styles (`GlassButtonStyle` / `GlassProminentButtonStyle`) are verified in the SDK. Apply:

- **`DetailView.swift`:** the **Sync** button → add `.buttonStyle(.glassProminent)`; **Edit Credentials…** and **Reveal in Finder** → `.buttonStyle(.glass)`; the destructive **Remove/Detach** button → `.buttonStyle(.glass)` and `.tint(.red)` (keep its existing `role: .destructive`).
- **`AddConnectionSheet.swift`:** **Add** → `.buttonStyle(.glassProminent)`; **Cancel** → `.buttonStyle(.glass)`.
- **`EditCredentialsSheet.swift`:** **Save** → `.buttonStyle(.glassProminent)`; **Cancel** → `.buttonStyle(.glass)`.
- **`EmptyStateView.swift`:** the **Choose Folder…** button → `.buttonStyle(.glassProminent)` and `.controlSize(.large)`.

Example (the modifier shape applied to an existing button — Sync in DetailView):
```swift
Button { sync.sync(connection) } label: {
    Label("Sync", systemImage: "arrow.triangle.2.circlepath")
}
.buttonStyle(.glassProminent)   // ← added
.disabled(isSyncing)            // existing
```

- [ ] **Step 1: Apply the button styles** to the four views per the list above (read each file, add the `.buttonStyle(...)` / `.tint(...)` / `.controlSize(...)` modifiers to the named buttons; change nothing else).

- [ ] **Step 2: Build + test**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO build test`
Expected: `** BUILD SUCCEEDED **` then `** TEST SUCCEEDED **` (16). If `.glass`/`.glassProminent` is rejected, the deployment target isn't 26 (Task 1) — stop and report.

- [ ] **Step 3: Commit**

```bash
git add macos/RossumLocal/Views/DetailView.swift macos/RossumLocal/Views/AddConnectionSheet.swift macos/RossumLocal/Views/EditCredentialsSheet.swift macos/RossumLocal/Views/EmptyStateView.swift
git commit -m "feat(macos): adopt Liquid Glass button styles on actions

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Sidebar scroll-edge effect + row refinement

**Files:** Modify `SidebarView.swift`

**Interfaces:**
- Consumes: the existing `List(store.connections, selection:)`.
- Produces: a Tahoe scroll-edge treatment + cleaner rows; selection/context-menu unchanged.

- [ ] **Step 1: Add the scroll-edge style + refine rows**

On the `List` in `SidebarView`, add (verified API — `.soft` style):
```swift
.scrollEdgeEffectStyle(.soft, for: .all)
```
Refine the row `HStack` for Tahoe spacing/legibility (keep `.tag(conn.id)`, the status glyph, and the `.contextMenu`):
```swift
HStack(spacing: 10) {
    VStack(alignment: .leading, spacing: 2) {
        Text(conn.name).font(.body)
        Text(conn.apiBase).font(.caption).foregroundStyle(.secondary).lineLimit(1)
    }
    Spacer(minLength: 8)
    statusGlyph(for: conn)
}
.padding(.vertical, 2)
```

- [ ] **Step 2: Build + test**

Run the build+test command (as Task 3 Step 2).
Expected: `** BUILD SUCCEEDED **` + `** TEST SUCCEEDED **` (16). If `scrollEdgeEffectStyle` is rejected, confirm the exact label is `scrollEdgeEffectStyle(_:for:)` (verified present) — do not substitute a non-existent `scrollEdgeEffect()`.

- [ ] **Step 3: Commit**

```bash
git add macos/RossumLocal/Views/SidebarView.swift
git commit -m "feat(macos): sidebar scroll-edge effect + refined rows

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: DetailView polish — spacing, typography, sync symbol

**Files:** Modify `DetailView.swift`

**Interfaces:**
- Produces: Tahoe spacing/typography + an animated sync symbol; no logic change.

- [ ] **Step 1: Tighten layout + add the sync symbol effect**

In `DetailView`:
- Bump the root `VStack(alignment: .leading, spacing: 16)` to `spacing: 20` and the field `Grid` `verticalSpacing` to `8`; keep `.textSelection(.enabled)` on values.
- In the `.started` arm of `syncStatus`, replace the bare `ProgressView()`-only treatment with an animated SF Symbol (verified `symbolEffect(_:isActive:)`):
```swift
case .started:
    HStack(spacing: 6) {
        Image(systemName: "arrow.triangle.2.circlepath")
            .symbolEffect(.rotate, isActive: true)
        Text("Syncing…")
    }
    .foregroundStyle(.secondary)
```
(If `.rotate` isn't available in this SDK, fall back to `.pulse` — both are indefinite symbol effects; the compile gate decides.)

- [ ] **Step 2 (optional, evaluate): background extension effect**

`backgroundExtensionEffect()` is verified-available but is designed for edge-to-edge content extending under the sidebar; on a form pane its effect is subtle. Apply it to the detail root container ONLY if it compiles cleanly and you judge it harmless:
```swift
.backgroundExtensionEffect()
```
If it produces no meaningful change or any layout oddity in the build logs, omit it and note that in the report. This is the one judgment call; everything else is deterministic.

- [ ] **Step 3: Build + test**

Run the build+test command. Expected: `** BUILD SUCCEEDED **` + `** TEST SUCCEEDED **` (16).

- [ ] **Step 4: Commit**

```bash
git add macos/RossumLocal/Views/DetailView.swift
git commit -m "feat(macos): detail-pane Tahoe spacing/typography + animated sync symbol

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Final clean build + docs

**Files:** Modify `macos/README.md`

- [ ] **Step 1: Final clean build + test**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO clean build test`
Expected: `** BUILD SUCCEEDED **` then `** TEST SUCCEEDED **` (16).

- [ ] **Step 2: Note the Tahoe target in `macos/README.md`**

Append:
```markdown

## Design

The UI targets **macOS 26 (Tahoe)** and adopts Liquid Glass: standard controls are
restyled automatically by the macOS 26 SDK, and primary actions use `.buttonStyle(.glass)` /
`.glassProminent`, the sidebar uses `.scrollEdgeEffectStyle(.soft)`, and the sync status uses
an animated SF Symbol. The app icon is a programmatic placeholder
(`tools/build-app-icon.sh`) pending a real Liquid Glass icon authored in Icon Composer.
```

- [ ] **Step 3: Commit**

```bash
git add macos/README.md
git commit -m "docs(macos): note Tahoe/Liquid Glass design + macOS 26 target

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage** (against `2026-06-30-tahoe-liquid-glass-modernization-design.md`):
- Deployment target → 26: Task 1. ✓
- Glass button styles on actions: Task 3. ✓
- Sidebar `scrollEdgeEffectStyle`: Task 4. ✓
- DetailView spacing/typography/`symbolEffect`: Task 5. ✓
- `backgroundExtensionEffect` (scoped optional, since it's for edge content): Task 5 Step 2 — a deliberate refinement of the spec's intent now that its exact purpose is known. ✓
- Placeholder app icon: Task 2. ✓
- Verified-APIs-only / no `glassEffect()`: enforced in Global Constraints + each task. ✓
- README note: Task 6. ✓
- No model changes / 16 tests green: every task's gate. ✓

**Placeholder scan:** No TBD/TODO. View edits are described as precise modifier additions to *named existing buttons/containers* (read-the-file-and-apply), with the exact modifier syntax shown — appropriate for a styling pass on existing code. The icon script is complete code. The one judgment call (`backgroundExtensionEffect`) is explicitly bounded.

**Type/API consistency:** all APIs match the SDK-verified signatures — `.buttonStyle(.glass)`/`.glassProminent`, `.scrollEdgeEffectStyle(.soft, for: .all)` (NOT `scrollEdgeEffect()`), `.backgroundExtensionEffect()`, `.symbolEffect(.rotate, isActive:)`. No `glassEffect()`/`GlassEffectContainer` anywhere.

**Verification reality:** styling has no new unit tests; the gate is compile + the unchanged 16 tests (regression guard). The rendered Liquid Glass appearance + the placeholder icon are MAINTAINER-verified on the controller's ad-hoc relaunch — only the maintainer can confirm the look.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-30-tahoe-liquid-glass-modernization.md`.
