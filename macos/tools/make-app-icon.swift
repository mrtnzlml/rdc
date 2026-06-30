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
