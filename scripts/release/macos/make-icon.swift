import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let side = 1024
let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(data: nil, width: side, height: side, bitsPerComponent: 8,
                    bytesPerRow: 0, space: colorSpace,
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!

let tile = CGPath(roundedRect: CGRect(x: 81, y: 81, width: 862, height: 862),
                  cornerWidth: 190, cornerHeight: 190, transform: nil)
ctx.addPath(tile)
ctx.setFillColor(red: 0.15, green: 0.25, blue: 0.42, alpha: 1)
ctx.fillPath()

// An open notebook above the workbench.
ctx.setFillColor(red: 0.97, green: 0.94, blue: 0.84, alpha: 1)
for direction in [-1.0, 1.0] as [CGFloat] {
    let page = CGMutablePath()
    page.move(to: CGPoint(x: 512 + direction * 18, y: 451))
    page.addQuadCurve(to: CGPoint(x: 512 + direction * 221, y: 539),
                      control: CGPoint(x: 512 + direction * 65, y: 539))
    page.addLine(to: CGPoint(x: 512 + direction * 221, y: 702))
    page.addQuadCurve(to: CGPoint(x: 512 + direction * 18, y: 620),
                      control: CGPoint(x: 512 + direction * 80, y: 705))
    page.closeSubpath()
    ctx.addPath(page)
}
ctx.fillPath()

ctx.setStrokeColor(red: 0.55, green: 0.79, blue: 0.76, alpha: 1)
ctx.setLineWidth(43)
ctx.setLineCap(.round)
for (start, end) in [(CGPoint(x: 272, y: 396), CGPoint(x: 752, y: 396)),
                     (CGPoint(x: 334, y: 390), CGPoint(x: 309, y: 302)),
                     (CGPoint(x: 690, y: 390), CGPoint(x: 715, y: 302))] {
    ctx.move(to: start)
    ctx.addLine(to: end)
}
ctx.strokePath()

let output = URL(fileURLWithPath: CommandLine.arguments[1])
let destination = CGImageDestinationCreateWithURL(output as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(destination, ctx.makeImage()!, nil)
precondition(CGImageDestinationFinalize(destination), "Could not write the fuzukue icon.")
