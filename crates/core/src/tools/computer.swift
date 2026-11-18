// GreenCode computer-use helper. One JSON command per stdin line, one JSON reply per stdout line.
// Apps are driven through the accessibility API and events posted to their pid, so the user's
// cursor, keyboard focus and frontmost window are left alone.
import AppKit
import ApplicationServices

struct Fail: Error { let message: String }

var elements: [AXUIElement] = []  // indexes handed out by the last state()
var targetPid: pid_t = 0
var windowOrigin = CGPoint.zero

func attr(_ e: AXUIElement, _ name: String) -> AnyObject? {
    var v: AnyObject?
    return AXUIElementCopyAttributeValue(e, name as CFString, &v) == .success ? v : nil
}

func frame(_ e: AXUIElement) -> CGRect? {
    guard let p = attr(e, "AXPosition"), let s = attr(e, "AXSize"),
          CFGetTypeID(p) == AXValueGetTypeID(), CFGetTypeID(s) == AXValueGetTypeID() else { return nil }
    var point = CGPoint.zero, size = CGSize.zero
    AXValueGetValue(p as! AXValue, .cgPoint, &point)
    AXValueGetValue(s as! AXValue, .cgSize, &size)
    return CGRect(origin: point, size: size)
}

func apps() -> [NSRunningApplication] {
    RunLoop.current.run(until: Date().addingTimeInterval(0.05))  // runningApplications only refreshes on the run loop
    return NSWorkspace.shared.runningApplications.filter { $0.activationPolicy == .regular }
}

func find(_ name: String) throws -> NSRunningApplication {
    let match = { apps().first { $0.localizedName?.caseInsensitiveCompare(name) == .orderedSame || $0.bundleIdentifier?.caseInsensitiveCompare(name) == .orderedSame } }
    if let app = match() { return app }
    let open = Process()
    open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
    open.arguments = ["-g", "-a", name]  // -g: launch without bringing it to the front
    open.standardError = FileHandle.nullDevice
    try open.run()
    open.waitUntilExit()
    for _ in 0..<50 {
        if let app = match() { usleep(500_000); return app }
        usleep(100_000)
    }
    throw Fail(message: "no app named `\(name)`; use list_apps")
}

func label(_ e: AXUIElement) -> String {
    var parts: [String] = []
    for name in ["AXTitle", "AXDescription", "AXValue", "AXPlaceholderValue"] {
        var text = ""
        if let s = attr(e, name) as? String { text = s } else if name == "AXValue", let n = attr(e, name) as? NSNumber { text = n.stringValue }
        text = text.replacingOccurrences(of: "\n", with: " ").trimmingCharacters(in: .whitespaces)
        if text.isEmpty || parts.contains(text) { continue }
        parts.append(text.count > 120 ? String(text.prefix(120)) + "…" : text)
    }
    return parts.joined(separator: " | ")
}

func walk(_ e: AXUIElement, _ depth: Int, _ lines: inout [String]) {
    if elements.count >= 500 || depth > 40 { return }
    let role = (attr(e, "AXRole") as? String ?? "AXUnknown").replacingOccurrences(of: "AX", with: "")
    let text = label(e)
    var next = depth
    // Unlabelled containers add depth and tokens but nothing to act on.
    if !(text.isEmpty && ["Group", "Unknown", "ScrollArea", "SplitGroup", "LayoutArea", "GenericElement"].contains(role)) {
        var line = String(repeating: " ", count: depth) + "\(elements.count) \(role)"
        if !text.isEmpty { line += " \"\(text)\"" }
        if attr(e, "AXEnabled") as? Bool == false { line += " (disabled)" }
        if role.hasPrefix("Text") || role == "ComboBox", attr(e, "AXFocused") as? Bool == true { line += " (focused)" }
        lines.append(line)
        elements.append(e)
        next += 1
    }
    for child in attr(e, "AXChildren") as? [AXUIElement] ?? [] { walk(child, next, &lines) }
}

func screenshot(_ pid: pid_t, _ bounds: CGRect) -> String? {
    let windows = CGWindowListCopyWindowInfo([.optionAll, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    // The window whose bounds are closest to the accessibility window's frame.
    let best = windows.filter { $0[kCGWindowOwnerPID as String] as? pid_t == pid && $0[kCGWindowLayer as String] as? Int == 0 }
        .compactMap { w -> (Int, CGFloat)? in
            guard let id = w[kCGWindowNumber as String] as? Int, let b = w[kCGWindowBounds as String] as? NSDictionary, let r = CGRect(dictionaryRepresentation: b) else { return nil }
            return (id, abs(r.minX - bounds.minX) + abs(r.minY - bounds.minY) + abs(r.width - bounds.width) + abs(r.height - bounds.height))
        }.min { $0.1 < $1.1 }
    guard let (id, distance) = best, distance < 8 else { return nil }
    let path = NSTemporaryDirectory() + "greencode-computer-\(getpid()).png"
    defer { try? FileManager.default.removeItem(atPath: path) }
    let capture = Process()
    capture.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
    capture.arguments = ["-x", "-o", "-l\(id)", path]
    capture.standardError = FileHandle.nullDevice
    guard (try? capture.run()) != nil else { return nil }
    capture.waitUntilExit()
    guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil), let image = CGImageSourceCreateImageAtIndex(source, 0, nil) else { return nil }
    // One pixel per point, so screenshot coordinates are window coordinates.
    let (w, h) = (Int(bounds.width), Int(bounds.height))
    guard w > 0, h > 0, let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return nil }
    ctx.interpolationQuality = .high
    ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
    guard let scaled = ctx.makeImage() else { return nil }
    return NSBitmapImageRep(cgImage: scaled).representation(using: .jpeg, properties: [.compressionFactor: 0.6])?.base64EncodedString()
}

func state(_ app: NSRunningApplication) throws -> [String: Any] {
    let pid = app.processIdentifier
    let ax = AXUIElementCreateApplication(pid)
    AXUIElementSetMessagingTimeout(ax, 3)
    // Chromium and Electron build their accessibility tree only when asked.
    AXUIElementSetAttributeValue(ax, "AXManualAccessibility" as CFString, kCFBooleanTrue)
    let window = attr(ax, "AXMainWindow") ?? attr(ax, "AXFocusedWindow") ?? (attr(ax, "AXWindows") as? [AXUIElement])?.first
    guard let window, CFGetTypeID(window) == AXUIElementGetTypeID() else { throw Fail(message: "\(app.localizedName ?? "app") has no open window") }
    let root = window as! AXUIElement
    elements = []
    targetPid = pid
    var lines: [String] = []
    walk(root, 0, &lines)
    let bounds = frame(root) ?? .zero
    windowOrigin = bounds.origin
    var text = "\(app.localizedName ?? "") — window \(Int(bounds.width))x\(Int(bounds.height))\n" + lines.joined(separator: "\n")
    if elements.count >= 500 { text += "\n(truncated at 500 elements)" }
    var reply: [String: Any] = [:]
    if let image = screenshot(pid, bounds) { reply["image"] = image }
    else if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess()
        text += "\n(no screenshot: Screen Recording permission is missing; the element list still works)"
    } else { text += "\n(no screenshot: the window is minimized or not capturable)" }
    reply["text"] = text
    return reply
}

func element(_ cmd: [String: Any], _ pid: pid_t) throws -> AXUIElement {
    guard let index = cmd["index"] as? Int else { throw Fail(message: "`index` is required") }
    guard pid == targetPid, elements.indices.contains(index) else { throw Fail(message: "no element \(index); call get_app_state for this app first") }
    return elements[index]
}

/// `index` → the element's center; otherwise window-relative `x`,`y` (screenshot coordinates).
func point(_ cmd: [String: Any], _ pid: pid_t, _ x: String = "x", _ y: String = "y") throws -> CGPoint {
    if x == "x", cmd["index"] != nil {
        guard let f = frame(try element(cmd, pid)) else { throw Fail(message: "element has no position") }
        return CGPoint(x: f.midX, y: f.midY)
    }
    guard let px = cmd[x] as? Double, let py = cmd[y] as? Double else { throw Fail(message: "`index` or `\(x)`/`\(y)` is required") }
    guard pid == targetPid else { throw Fail(message: "call get_app_state for this app first") }
    return CGPoint(x: windowOrigin.x + px, y: windowOrigin.y + py)
}

func mouse(_ type: CGEventType, _ at: CGPoint, _ button: CGMouseButton, _ pid: pid_t, click: Int64 = 1) {
    guard let ev = CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: at, mouseButton: button) else { return }
    ev.setIntegerValueField(.mouseEventClickState, value: click)
    ev.postToPid(pid)
    usleep(30_000)
}

let keyCodes: [String: CGKeyCode] = [
    "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9, "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17,
    "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "=": 24, "9": 25, "7": 26, "-": 27, "8": 28, "0": 29, "]": 30, "o": 31, "u": 32, "[": 33,
    "i": 34, "p": 35, "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42, ",": 43, "/": 44, "n": 45, "m": 46, ".": 47, "`": 50,
    "return": 36, "enter": 36, "tab": 48, "space": 49, "delete": 51, "backspace": 51, "escape": 53, "esc": 53, "forwarddelete": 117,
    "home": 115, "end": 119, "pageup": 116, "pagedown": 121, "left": 123, "right": 124, "down": 125, "up": 126,
]
let modifiers: [String: CGEventFlags] = [
    "cmd": .maskCommand, "command": .maskCommand, "shift": .maskShift, "alt": .maskAlternate, "option": .maskAlternate, "ctrl": .maskControl, "control": .maskControl,
]

func act(_ cmd: [String: Any]) throws -> [String: Any] {
    guard let action = cmd["action"] as? String else { throw Fail(message: "`action` is required") }
    if action == "list_apps" {
        return ["text": apps().map { "\($0.localizedName ?? "?")\t\($0.bundleIdentifier ?? "")" }.joined(separator: "\n")]
    }
    guard AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": true] as CFDictionary) else {
        throw Fail(message: "Accessibility permission is missing. Allow GreenCode (or the terminal running it) in System Settings > Privacy & Security > Accessibility, then retry.")
    }
    guard let name = cmd["app"] as? String else { throw Fail(message: "`app` is required") }
    let app = try find(name)
    let pid = app.processIdentifier
    switch action {
    case "get_app_state": break
    case "click":
        let count = cmd["count"] as? Int ?? 1
        let right = cmd["button"] as? String == "right"
        // AXPress needs no pointer at all; fall back to posted mouse events for everything else.
        if !right, count == 1, cmd["index"] != nil, AXUIElementPerformAction(try element(cmd, pid), "AXPress" as CFString) == .success { break }
        let at = try point(cmd, pid)
        for i in 1...max(1, count) {
            mouse(right ? .rightMouseDown : .leftMouseDown, at, right ? .right : .left, pid, click: Int64(i))
            mouse(right ? .rightMouseUp : .leftMouseUp, at, right ? .right : .left, pid, click: Int64(i))
        }
    case "perform_action":
        guard let ax = cmd["ax_action"] as? String else { throw Fail(message: "`ax_action` is required") }
        let result = AXUIElementPerformAction(try element(cmd, pid), ax as CFString)
        guard result == .success else { throw Fail(message: "\(ax) failed (AXError \(result.rawValue))") }
    case "set_value":
        guard let value = cmd["text"] as? String else { throw Fail(message: "`text` is required") }
        let result = AXUIElementSetAttributeValue(try element(cmd, pid), "AXValue" as CFString, value as CFString)
        guard result == .success else { throw Fail(message: "the element's value cannot be set (AXError \(result.rawValue)); click it and use type_text") }
    case "type_text":
        guard let text = cmd["text"] as? String else { throw Fail(message: "`text` is required") }
        let units = Array(text.utf16)
        for start in stride(from: 0, to: units.count, by: 20) {
            let chunk = Array(units[start..<min(start + 20, units.count)])
            for down in [true, false] {
                guard let ev = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: down) else { continue }
                ev.keyboardSetUnicodeString(stringLength: chunk.count, unicodeString: chunk)
                ev.postToPid(pid)
            }
            usleep(10_000)
        }
    case "press_key":
        guard let combo = cmd["key"] as? String else { throw Fail(message: "`key` is required") }
        var parts = combo.lowercased().split(separator: "+").map(String.init)
        if combo.hasSuffix("+") { parts.append("+") }
        guard let key = parts.popLast(), let code = keyCodes[key] ?? (key == "+" ? 24 : nil) else { throw Fail(message: "unknown key in `\(combo)`") }
        var flags: CGEventFlags = key == "+" ? .maskShift : []
        for m in parts {
            guard let flag = modifiers[m] else { throw Fail(message: "unknown modifier `\(m)`") }
            flags.insert(flag)
        }
        for down in [true, false] {
            guard let ev = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down) else { continue }
            ev.flags = flags
            ev.postToPid(pid)
            usleep(20_000)
        }
    case "scroll":
        let at = try point(cmd, pid)
        let (dx, dy) = (cmd["dx"] as? Double ?? 0, cmd["dy"] as? Double ?? 0)
        // Positive dy scrolls the content down, like a page-down.
        guard let ev = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 2, wheel1: Int32(-dy), wheel2: Int32(-dx), wheel3: 0) else { break }
        ev.location = at
        ev.postToPid(pid)
    case "drag":
        let (from, to) = (try point(cmd, pid), try point(cmd, pid, "to_x", "to_y"))
        mouse(.leftMouseDown, from, .left, pid)
        for step in 1...10 {
            let t = CGFloat(step) / 10
            mouse(.leftMouseDragged, CGPoint(x: from.x + (to.x - from.x) * t, y: from.y + (to.y - from.y) * t), .left, pid)
        }
        mouse(.leftMouseUp, to, .left, pid)
    default: throw Fail(message: "unknown action `\(action)`")
    }
    if action != "get_app_state" { usleep(400_000) }  // let the UI settle before reporting it
    return try state(app)
}

while let line = readLine() {
    var reply: [String: Any]
    do {
        guard let cmd = try JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any] else { throw Fail(message: "expected a JSON object") }
        reply = try act(cmd)
    } catch let e as Fail { reply = ["error": e.message] } catch { reply = ["error": "\(error)"] }
    let data = (try? JSONSerialization.data(withJSONObject: reply)) ?? Data("{\"error\":\"unencodable reply\"}".utf8)
    FileHandle.standardOutput.write(data + Data("\n".utf8))
}
