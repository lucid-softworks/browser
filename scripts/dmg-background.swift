// Draw the installer artwork without depending on a browser, image service, or Finder UI.
// Usage: swift dmg-background.swift OUTPUT.png
import AppKit

guard CommandLine.arguments.count == 2 else {
    fatalError("Usage: swift dmg-background.swift OUTPUT.png")
}
let width = 640
let height = 420
guard let bitmap = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: width * 2, pixelsHigh: height * 2,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
), let graphics = NSGraphicsContext(bitmapImageRep: bitmap) else {
    fatalError("Cannot create installer background")
}
bitmap.size = NSSize(width: CGFloat(width), height: CGFloat(height))
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = graphics
graphics.cgContext.scaleBy(x: 2, y: 2)

let bounds = NSRect(x: 0, y: 0, width: CGFloat(width), height: CGFloat(height))
let background = NSGradient(colors: [
    NSColor(calibratedRed: 0.98, green: 0.98, blue: 1.0, alpha: 1),
    NSColor(calibratedRed: 0.93, green: 0.95, blue: 0.99, alpha: 1)
])!
background.draw(in: bounds, angle: -90)

func text(_ value: String, top: CGFloat, size: CGFloat, weight: NSFont.Weight, color: NSColor) {
    let paragraph = NSMutableParagraphStyle()
    paragraph.alignment = .center
    (value as NSString).draw(in: NSRect(x: 32, y: CGFloat(height) - top - size * 1.5,
                                     width: CGFloat(width) - 64, height: size * 1.5),
                            withAttributes: [.font: NSFont.systemFont(ofSize: size, weight: weight),
                                             .foregroundColor: color, .paragraphStyle: paragraph])
}
let ink = NSColor(calibratedRed: 0.13, green: 0.17, blue: 0.27, alpha: 1)
let muted = NSColor(calibratedRed: 0.38, green: 0.43, blue: 0.53, alpha: 1)
text("Install Browser", top: 38, size: 30, weight: .semibold, color: ink)
text("Drag Browser to Applications", top: 87, size: 16, weight: .regular, color: muted)

// Finder places its actual app/folder icons over these softly outlined wells.
for x in [CGFloat(170), CGFloat(470)] {
    let well = NSBezierPath(roundedRect: NSRect(x: x - 75, y: 135, width: 150, height: 154),
                            xRadius: 24, yRadius: 24)
    NSColor.white.withAlphaComponent(0.7).setFill()
    well.fill()
    NSColor(calibratedRed: 0.82, green: 0.86, blue: 0.94, alpha: 1).setStroke()
    well.lineWidth = 1
    well.stroke()
}
let arrow = NSBezierPath()
arrow.move(to: NSPoint(x: 276, y: 215))
arrow.line(to: NSPoint(x: 364, y: 215))
arrow.move(to: NSPoint(x: 346, y: 233))
arrow.line(to: NSPoint(x: 364, y: 215))
arrow.line(to: NSPoint(x: 346, y: 197))
arrow.lineWidth = 5
arrow.lineCapStyle = .round
arrow.lineJoinStyle = .round
NSColor(calibratedRed: 0.34, green: 0.40, blue: 0.77, alpha: 1).setStroke()
arrow.stroke()
text("Then open Browser from Applications.", top: 334, size: 14, weight: .medium, color: muted)

NSGraphicsContext.restoreGraphicsState()
guard let png = bitmap.representation(using: .png, properties: [:]) else {
    fatalError("Cannot encode installer background")
}
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
