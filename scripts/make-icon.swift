// Renders assets/icon/AppIcon.svg into the formats macOS and GitHub need:
// AppIcon.icns (all iconset sizes) and the PNGs used by the README.
//
//   swift scripts/make-icon.swift
//
// Needs macOS 14 or later, where NSImage reads SVG.

import AppKit

let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
let dir = root.appendingPathComponent("assets/icon")
guard let svg = NSImage(contentsOf: dir.appendingPathComponent("AppIcon.svg")) else {
    fatalError("cannot read AppIcon.svg")
}

func png(_ size: Int, to url: URL) throws {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: size, height: size)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current?.imageInterpolation = .high
    svg.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
    NSGraphicsContext.restoreGraphicsState()
    try rep.representation(using: .png, properties: [:])!.write(to: url)
}

let iconset = FileManager.default.temporaryDirectory.appendingPathComponent("AppIcon.iconset")
try? FileManager.default.removeItem(at: iconset)
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for points in [16, 32, 128, 256, 512] {
    try png(points, to: iconset.appendingPathComponent("icon_\(points)x\(points).png"))
    try png(points * 2, to: iconset.appendingPathComponent("icon_\(points)x\(points)@2x.png"))
}

let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["-c", "icns", iconset.path, "-o", dir.appendingPathComponent("AppIcon.icns").path]
try iconutil.run()
iconutil.waitUntilExit()
guard iconutil.terminationStatus == 0 else { fatalError("iconutil failed") }

try png(1024, to: dir.appendingPathComponent("AppIcon-1024.png"))
try png(256, to: dir.appendingPathComponent("AppIcon-256.png"))
print("wrote AppIcon.icns, AppIcon-1024.png, AppIcon-256.png in \(dir.path)")
