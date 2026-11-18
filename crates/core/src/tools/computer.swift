// GreenCode computer-use helper. One JSON command per stdin line, one JSON reply per stdout line.
// AX actions and window-addressed input avoid using the shared cursor or activating the target.
import AppKit
import ApplicationServices

struct Fail: Error { let message: String }

// Element indexes stay stable while the same window is inspected, so a state can be sent as a diff.
var ids: [AXUIElement: Int] = [:]
var elements: [Int: AXUIElement] = [:]
var shown: [Int: String] = [:]  // last reported line (indent + description) per index
var nextId = 0
var targetPid: pid_t = 0
var lastWindow: AXUIElement?
var capturedWindow: WindowTarget?

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

struct Installed { let name: String; let id: String; let url: URL; let lastUsed: Date?; let uses: Int? }

func installed() -> [Installed] {
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    let dirs = ["/Applications", "/Applications/Utilities", "/System/Applications", "/System/Applications/Utilities", home + "/Applications"]
    return dirs.flatMap { dir in
        ((try? FileManager.default.contentsOfDirectory(atPath: dir)) ?? []).filter { $0.hasSuffix(".app") }.compactMap { file -> Installed? in
            let url = URL(fileURLWithPath: dir).appendingPathComponent(file)
            guard let id = Bundle(url: url)?.bundleIdentifier else { return nil }
            let item = MDItemCreateWithURL(nil, url as CFURL)
            return Installed(
                name: FileManager.default.displayName(atPath: url.path).replacingOccurrences(of: ".app", with: ""), id: id, url: url,
                lastUsed: item.flatMap { MDItemCopyAttribute($0, kMDItemLastUsedDate) as? Date },
                uses: item.flatMap { MDItemCopyAttribute($0, "kMDItemUseCount" as CFString) as? Int })
        }
    }
}

func listApps() -> String {
    let running = apps()
    var seen = Set<String>()
    var rows: [(name: String, id: String, running: Bool, lastUsed: Date?, uses: Int?)] = []
    for app in installed() where seen.insert(app.id).inserted {
        rows.append((app.name, app.id, running.contains { $0.bundleIdentifier == app.id }, app.lastUsed, app.uses))
    }
    for app in running where seen.insert(app.bundleIdentifier ?? "").inserted {
        rows.append((app.localizedName ?? "?", app.bundleIdentifier ?? "", true, nil, nil))
    }
    rows.sort { ($0.running ? 1 : 0, $0.lastUsed ?? .distantPast) > ($1.running ? 1 : 0, $1.lastUsed ?? .distantPast) }
    let day = DateFormatter()
    day.dateFormat = "yyyy-MM-dd"
    return rows.map { row in
        var line = "\(row.name)\t\(row.id)"
        if row.running { line += "\trunning" }
        if let date = row.lastUsed { line += "\tlast used \(day.string(from: date))" }
        if let uses = row.uses { line += ", \(uses) uses" }
        return line
    }.joined(separator: "\n")
}

/// `app` is a display name, bundle identifier or app path; a closed app is launched in the background.
func find(_ name: String) throws -> NSRunningApplication {
    let path = name.hasSuffix("/") ? String(name.dropLast()) : name
    let match = {
        apps().first {
            $0.localizedName?.caseInsensitiveCompare(name) == .orderedSame || $0.bundleIdentifier?.caseInsensitiveCompare(name) == .orderedSame
                || $0.bundleURL?.path == path
        }
    }
    if let app = match() { return app }
    let url = path.hasPrefix("/") ? URL(fileURLWithPath: path)
        : NSWorkspace.shared.urlForApplication(withBundleIdentifier: name)
            ?? installed().first { $0.name.caseInsensitiveCompare(name) == .orderedSame }?.url
    let open = Process()
    open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
    open.arguments = ["-g", "-a", url?.path ?? name]  // -g: launch without bringing it to the front
    open.standardError = FileHandle.nullDevice
    try open.run()
    open.waitUntilExit()
    let id = url.flatMap { Bundle(url: $0)?.bundleIdentifier }
    for _ in 0..<50 {
        if let app = match() ?? apps().first(where: { id != nil && $0.bundleIdentifier == id }) { usleep(500_000); return app }
        usleep(100_000)
    }
    throw Fail(message: "no app named `\(name)`; use list_apps and pass its bundle identifier")
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

/// Accessibility actions besides a plain press, raw names; shown without the `AX` prefix.
func secondary(_ e: AXUIElement) -> [String] {
    var names: CFArray?
    guard AXUIElementCopyActionNames(e, &names) == .success, let list = names as? [String] else { return [] }
    return list.filter { !["AXPress", "AXScrollToVisible", "AXShowDefaultUI", "AXShowAlternateUI", "AXRaise"].contains($0) && !$0.contains(":") }
}

func display(_ action: String) -> String { action.hasPrefix("AX") ? String(action.dropFirst(2)) : action }

struct Node { let e: AXUIElement; let line: String }  // line: indent + description, without the index

func walk(_ e: AXUIElement, _ depth: Int, _ nodes: inout [Node]) {
    if nodes.count >= 500 || depth > 40 { return }
    let role = (attr(e, "AXRole") as? String ?? "AXUnknown").replacingOccurrences(of: "AX", with: "")
    let text = label(e)
    var next = depth
    // Unlabelled containers add depth and tokens but nothing to act on.
    if !(text.isEmpty && ["Group", "Unknown", "ScrollArea", "SplitGroup", "LayoutArea", "GenericElement"].contains(role)) {
        var line = String(repeating: " ", count: depth) + role
        if !text.isEmpty { line += " \"\(text)\"" }
        if attr(e, "AXEnabled") as? Bool == false { line += " (disabled)" }
        if role.hasPrefix("Text") || role == "ComboBox", attr(e, "AXFocused") as? Bool == true { line += " (focused)" }
        if attr(e, "AXSelected") as? Bool == true { line += " (selected)" }
        let actions = secondary(e).map(display)
        if !actions.isEmpty { line += " [\(actions.joined(separator: ", "))]" }
        nodes.append(Node(e: e, line: line))
        next += 1
    }
    for child in attr(e, "AXChildren") as? [AXUIElement] ?? [] { walk(child, next, &nodes) }
}

struct WindowTarget {
    let id: Int
    let bounds: CGRect
}

func windowTarget(_ pid: pid_t, _ bounds: CGRect) -> WindowTarget? {
    let windows = CGWindowListCopyWindowInfo([.optionAll, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    // Refuse ambiguous windows rather than send input to a sibling window at the same position.
    let matches = windows.compactMap { w -> WindowTarget? in
        guard w[kCGWindowOwnerPID as String] as? pid_t == pid, w[kCGWindowLayer as String] as? Int == 0,
            let id = w[kCGWindowNumber as String] as? Int, let b = w[kCGWindowBounds as String] as? NSDictionary,
            let r = CGRect(dictionaryRepresentation: b), !r.isEmpty,
            abs(r.minX - bounds.minX) + abs(r.minY - bounds.minY) + abs(r.width - bounds.width) + abs(r.height - bounds.height) < 8
        else { return nil }
        return WindowTarget(id: id, bounds: r)
    }
    return matches.count == 1 ? matches.first : nil
}

func screenshot(_ target: WindowTarget) -> String? {
    let (id, bounds) = (target.id, target.bounds)
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

func state(_ app: NSRunningApplication, settle: Bool, full: Bool) throws -> [String: Any] {
    let pid = app.processIdentifier
    let ax = AXUIElementCreateApplication(pid)
    AXUIElementSetMessagingTimeout(ax, 3)
    // Chromium and Electron build their accessibility tree only when asked.
    AXUIElementSetAttributeValue(ax, "AXManualAccessibility" as CFString, kCFBooleanTrue)
    // The main window can be briefly missing while the app handles an event.
    var window: AnyObject?
    for attempt in 0..<5 {
        window = attr(ax, "AXMainWindow") ?? attr(ax, "AXFocusedWindow") ?? (attr(ax, "AXWindows") as? [AXUIElement])?.first
        if window != nil || attempt == 4 { break }
        usleep(200_000)
    }
    guard let window, CFGetTypeID(window) == AXUIElementGetTypeID() else { throw Fail(message: "\(app.localizedName ?? "app") has no open window") }
    let root = window as! AXUIElement
    let snapshot = { () -> [Node] in var nodes: [Node] = []; walk(root, 0, &nodes); return nodes }
    // After an action, wait until the tree stops changing and no progress indicator is left (at most ~5 s).
    if settle { usleep(500_000) }
    var nodes = snapshot()
    let deadline = Date().addingTimeInterval(4.5)
    while settle, Date() < deadline {
        usleep(300_000)
        let next = snapshot()
        let busy = next.contains { $0.line.contains("ProgressIndicator") || $0.line.contains("BusyIndicator") }
        let same = next.map(\.line) == nodes.map(\.line)
        nodes = next
        if same && !busy { break }
    }
    if pid != targetPid || lastWindow.map({ !CFEqual($0, root) }) ?? true {
        (ids, shown, nextId) = ([:], [:], 0)
    }
    let diff = !full && !shown.isEmpty
    var current: [Int: String] = [:]
    var lines: [String] = []
    elements = [:]
    for node in nodes {
        let id = ids[node.e] ?? nextId
        if id == nextId { nextId += 1; ids[node.e] = id }
        guard current[id] == nil else { continue }
        current[id] = node.line
        elements[id] = node.e
        let indent = node.line.prefix { $0 == " " }
        lines.append("\(indent)\(id) \(node.line.dropFirst(indent.count))")
    }
    ids = ids.filter { current[$0.value] != nil }
    let row = { (sign: String, id: Int, line: String) in "\(sign) \(id) \(line.trimmingCharacters(in: .whitespaces))" }
    let changes = shown.keys.filter { current[$0] == nil }.sorted().map { row("-", $0, shown[$0]!) }
        + current.keys.filter { shown[$0] != current[$0] }.sorted().map { row(shown[$0] == nil ? "+" : "~", $0, current[$0]!) }
    (shown, targetPid, lastWindow) = (current, pid, root)
    let bounds = frame(root) ?? .zero
    capturedWindow = windowTarget(pid, bounds)
    var text = "\(app.localizedName ?? "") — \(attr(root, "AXTitle") as? String ?? "") \(Int(bounds.width))x\(Int(bounds.height))\n"
    if diff && changes.count * 2 < lines.count {
        text += changes.isEmpty ? "No element changes since the last state." : "Changes since the last state (- removed, + added, ~ changed; disable_diff for the full tree):\n" + changes.joined(separator: "\n")
    } else {
        text += lines.joined(separator: "\n")
    }
    if nodes.count >= 500 { text += "\n(truncated at 500 elements)" }
    var reply: [String: Any] = [:]
    if let image = capturedWindow.flatMap(screenshot) { reply["image"] = image }
    else if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess()
        text += "\n(no screenshot: Screen Recording permission is missing; the element list still works)"
    } else { text += "\n(no screenshot: the window is minimized or not capturable)" }
    reply["text"] = text
    return reply
}

func element(_ cmd: [String: Any], _ pid: pid_t) throws -> AXUIElement {
    guard let index = cmd["element_index"] as? Int else { throw Fail(message: "`element_index` is required") }
    guard pid == targetPid, let e = elements[index] else { throw Fail(message: "no element \(index); call get_app_state for this app first") }
    return e
}

func pointerTarget(_ pid: pid_t) throws -> WindowTarget {
    guard CGPreflightPostEventAccess() else { throw Fail(message: "macOS has not granted permission to post input events") }
    guard pid == targetPid, let capturedWindow, let bounds = lastWindow.flatMap(frame),
        let current = windowTarget(pid, bounds), current.id == capturedWindow.id, current.bounds.size == capturedWindow.bounds.size
    else { throw Fail(message: "target window changed or cannot be identified; call get_app_state again") }
    return current
}

/// Convert screenshot pixels to screen coordinates relative to the current window origin.
func point(_ cmd: [String: Any], _ pid: pid_t, _ window: WindowTarget, _ x: String = "x", _ y: String = "y") throws -> CGPoint {
    let p: CGPoint
    if x == "x", cmd["element_index"] != nil {
        guard let f = frame(try element(cmd, pid)) else { throw Fail(message: "element has no position") }
        p = CGPoint(x: f.midX, y: f.midY)
    } else {
        guard let px = cmd[x] as? Double, let py = cmd[y] as? Double else {
            throw Fail(message: "`element_index` or `\(x)`/`\(y)` is required")
        }
        p = CGPoint(x: window.bounds.minX + px, y: window.bounds.minY + py)
    }
    guard window.bounds.contains(p) else { throw Fail(message: "point is outside the target window") }
    return p
}

private let skyLight = dlopen("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight", RTLD_LAZY | RTLD_LOCAL)
private func eventSymbol<T>(_ name: String, _ type: T.Type) -> T? {
    guard let skyLight, let symbol = dlsym(skyLight, name) else { return nil }
    return unsafeBitCast(symbol, to: type)
}
private let postWindowEvent = eventSymbol("SLEventPostToPid", (@convention(c) (pid_t, CGEvent) -> Void).self)
private let setWindowLocation = eventSymbol("CGEventSetWindowLocation", (@convention(c) (CGEvent, CGPoint) -> Void).self)
private let setEventField = eventSymbol("SLEventSetIntegerValueField", (@convention(c) (CGEvent, UInt32, Int64) -> Void).self)

func routePointer(_ event: CGEvent, _ at: CGPoint, _ window: WindowTarget, _ pid: pid_t) {
    event.location = at
    event.setIntegerValueField(.mouseEventWindowUnderMousePointer, value: Int64(window.id))
    event.setIntegerValueField(.mouseEventWindowUnderMousePointerThatCanHandleThisEvent, value: Int64(window.id))
    // SkyLight uses top-left window coordinates; NSEvent's constructor uses bottom-left.
    let local = CGPoint(x: at.x - window.bounds.minX, y: at.y - window.bounds.minY)
    if let postWindowEvent, let setWindowLocation, let setEventField {
        setWindowLocation(event, local)
        setEventField(event, 51, Int64(window.id)) // NSEvent windowNumber, also needed for wheel events.
        postWindowEvent(pid, event)
    } else { event.postToPid(pid) }
}

func mouse(_ type: NSEvent.EventType, _ at: CGPoint, _ button: CGMouseButton, _ window: WindowTarget, _ pid: pid_t,
           number: Int, click: Int = 1, flags: CGEventFlags = []) throws {
    let local = CGPoint(x: at.x - window.bounds.minX, y: window.bounds.maxY - at.y)
    guard let ev = NSEvent.mouseEvent(with: type, location: local, modifierFlags: .init(rawValue: UInt(flags.rawValue)),
        timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.id, context: nil, eventNumber: number,
        clickCount: click, pressure: 1)?.cgEvent else { throw Fail(message: "cannot construct a window mouse event") }
    ev.setIntegerValueField(.mouseEventButtonNumber, value: Int64(button.rawValue))
    routePointer(ev, at, window, pid)
    usleep(30_000)
}

func post(_ code: CGKeyCode, _ flags: CGEventFlags, _ pid: pid_t, hold: Double = 0) {
    for down in [true, false] {
        guard let ev = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down) else { continue }
        ev.flags = flags
        ev.postToPid(pid)
        usleep(down && hold > 0 ? useconds_t(hold * 1_000_000) : 20_000)
    }
}

func type(_ text: String, _ pid: pid_t) {
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
}

// xdotool key names, lowercased without underscores, to macOS virtual key codes.
let keyCodes: [String: CGKeyCode] = [
    "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9, "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17,
    "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "=": 24, "9": 25, "7": 26, "-": 27, "8": 28, "0": 29, "]": 30, "o": 31, "u": 32, "[": 33,
    "i": 34, "p": 35, "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42, ",": 43, "/": 44, "n": 45, "m": 46, ".": 47, "`": 50,
    "equal": 24, "minus": 27, "bracketright": 30, "bracketleft": 33, "apostrophe": 39, "semicolon": 41, "backslash": 42, "comma": 43, "slash": 44,
    "period": 47, "grave": 50, "return": 36, "enter": 36, "tab": 48, "space": 49, "backspace": 51, "escape": 53, "esc": 53, "delete": 117,
    "help": 114, "insert": 114, "home": 115, "end": 119, "prior": 116, "pageup": 116, "next": 121, "pagedown": 121,
    "left": 123, "right": 124, "down": 125, "up": 126,
    "f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97, "f7": 98, "f8": 100, "f9": 101, "f10": 109, "f11": 103, "f12": 111,
    "kp0": 82, "kp1": 83, "kp2": 84, "kp3": 85, "kp4": 86, "kp5": 87, "kp6": 88, "kp7": 89, "kp8": 91, "kp9": 92,
    "kpdecimal": 65, "kpmultiply": 67, "kpadd": 69, "kpsubtract": 78, "kpdivide": 75, "kpequal": 81, "kpenter": 76,
]
let modifiers: [String: CGEventFlags] = [
    "super": .maskCommand, "cmd": .maskCommand, "command": .maskCommand, "meta": .maskCommand, "shift": .maskShift,
    "alt": .maskAlternate, "option": .maskAlternate, "ctrl": .maskControl, "control": .maskControl,
]

func flags(_ names: [String]) throws -> CGEventFlags {
    try names.reduce(into: []) { all, m in
        guard let flag = modifiers[m.lowercased()] else { throw Fail(message: "unknown modifier `\(m)`") }
        all.insert(flag)
    }
}

/// Returns a note when the shortcut's menu item is disabled because the app is in the background.
func pressKey(_ combo: String, _ pid: pid_t, hold: Double) throws -> String? {
    var parts = combo.split(separator: "+").map(String.init)
    if combo.hasSuffix("+") { parts.append("+") }
    guard let key = parts.popLast() else { throw Fail(message: "`key` is required") }
    var mods = try flags(parts)
    let name = key.count == 1 ? key.lowercased() : key.lowercased().replacingOccurrences(of: "_", with: "")
    if key.count == 1, key != key.lowercased() { mods.insert(.maskShift) }
    if let code = keyCodes[name] ?? (key == "+" ? 24 : nil) {
        if key == "+" { mods.insert(.maskShift) }
        // Background apps often ignore posted Cmd shortcuts; their menu items work regardless.
        let menu = key.count == 1 && hold == 0 ? menuKey(pid, key, mods) : ""
        guard let menu else { return nil }
        if name == "a", mods == .maskCommand, hold == 0,
            NSWorkspace.shared.frontmostApplication?.processIdentifier != pid, selectAllText(pid) { return nil }
        post(code, mods, pid, hold: hold)
        if !menu.isEmpty { return "(menu item \"\(menu)\" is disabled while the app is in the background; the key was sent anyway — prefer element actions)" }
    } else if key.count == 1, parts.isEmpty {
        type(key, pid)
    } else {
        throw Fail(message: "unknown key in `\(combo)`")
    }
    return nil
}

private func selectAllText(_ pid: pid_t) -> Bool {
    let app = AXUIElementCreateApplication(pid)
    guard let focused = attr(app, "AXFocusedUIElement"), CFGetTypeID(focused) == AXUIElementGetTypeID() else { return false }
    let field = focused as! AXUIElement
    guard let role = attr(field, "AXRole") as? String, role == "AXTextArea" || role == "AXTextField",
        attr(field, "AXSubrole") as? String != "AXSecureTextField",
        let value = attr(field, "AXValue") as? String else { return false }
    var range = CFRange(location: 0, length: value.utf16.count)
    guard let selection = AXValueCreate(.cfRange, &range) else { return false }
    return AXUIElementSetAttributeValue(field, "AXSelectedTextRange" as CFString, selection) == .success
}

/// Presses the app's menu item bound to `char` with `mods`; the Apple menu (log out, lock screen) is never searched.
/// Returns nil when pressed, otherwise the title of a matching but disabled item ("" when there is none).
func menuKey(_ pid: pid_t, _ char: String, _ mods: CGEventFlags) -> String? {
    guard mods.contains(.maskCommand), let bar = attr(AXUIElementCreateApplication(pid), "AXMenuBar"),
        CFGetTypeID(bar) == AXUIElementGetTypeID() else { return "" }
    let mask = (mods.contains(.maskShift) ? 1 : 0) | (mods.contains(.maskAlternate) ? 2 : 0) | (mods.contains(.maskControl) ? 4 : 0)
    func search(_ e: AXUIElement, _ depth: Int) -> AXUIElement? {
        if (attr(e, "AXMenuItemCmdChar") as? String)?.caseInsensitiveCompare(char) == .orderedSame,
            attr(e, "AXMenuItemCmdModifiers") as? Int ?? 0 == mask { return e }
        guard depth < 4 else { return nil }
        return (attr(e, "AXChildren") as? [AXUIElement] ?? []).lazy.compactMap { search($0, depth + 1) }.first
    }
    let menus = (attr(bar as! AXUIElement, "AXChildren") as? [AXUIElement] ?? []).dropFirst()
    guard let item = menus.lazy.compactMap({ search($0, 1) }).first else { return "" }
    if attr(item, "AXEnabled") as? Bool == false { return attr(item, "AXTitle") as? String ?? "" }
    return AXUIElementPerformAction(item, "AXPress" as CFString) == .success ? nil : ""
}

/// Scrolls the scroll area holding `e` through its scroll bar; false when there is none.
func scrollAX(_ e: AXUIElement, vertical: Bool, by pages: Double) -> Bool {
    var area: AXUIElement? = e
    while let a = area, attr(a, "AXRole") as? String != "AXScrollArea" { area = attr(a, "AXParent").map { $0 as! AXUIElement } }
    guard let area, let bar = attr(area, vertical ? "AXVerticalScrollBar" : "AXHorizontalScrollBar").map({ $0 as! AXUIElement }),
        let value = attr(bar, "AXValue") as? Double, let view = frame(area),
        let content = (attr(area, "AXContents") as? [AXUIElement])?.first.flatMap(frame) else { return false }
    let range = vertical ? content.height - view.height : content.width - view.width
    guard range > 0 else { return true }
    let next = min(1, max(0, value + pages * 0.8 * (vertical ? view.height : view.width) / range))
    return AXUIElementSetAttributeValue(bar, "AXValue" as CFString, NSNumber(value: next)) == .success
}

/// Returns a note when the app is in the background and only plain text could be inserted.
func paste(_ text: String, _ format: String, _ pid: pid_t) throws -> String? {
    var plain = text, rich: (Data, NSPasteboard.PasteboardType)?
    switch format {
    case "text": break
    case "md":
        // shortcut: inline Markdown only (emphasis, code, links, line breaks); block syntax pastes as typed.
        let styled = try NSAttributedString(markdown: text, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))
        plain = styled.string
        rich = styled.rtf(from: NSRange(location: 0, length: styled.length)).map { ($0, .rtf) }
    case "html":
        plain = text.replacingOccurrences(of: "<[^>]+>", with: "", options: .regularExpression)
        rich = (Data(text.utf8), .html)
    default: throw Fail(message: "`format` must be text, md or html")
    }
    let board = NSPasteboard.general
    let saved = (board.pasteboardItems ?? []).map { item -> NSPasteboardItem in
        let copy = NSPasteboardItem()
        for t in item.types { if let data = item.data(forType: t) { copy.setData(data, forType: t) } }
        return copy
    }
    board.clearContents()
    if let (data, type) = rich { board.setData(data, forType: type) }
    board.setString(plain, forType: .string)
    defer {
        usleep(500_000)  // the app reads the pasteboard asynchronously
        board.clearContents()
        board.writeObjects(saved)
    }
    if menuKey(pid, "V", .maskCommand) == nil { return nil }
    // An inactive app disables Edit > Paste; insert into the focused text element instead.
    if let focused = attr(AXUIElementCreateApplication(pid), "AXFocusedUIElement"), CFGetTypeID(focused) == AXUIElementGetTypeID(),
        AXUIElementSetAttributeValue(focused as! AXUIElement, "AXSelectedText" as CFString, plain as CFString) == .success {
        return rich == nil ? nil : "(the app is in the background, so the text was inserted without formatting)"
    }
    post(9, .maskCommand, pid)
    return nil
}

func selectText(_ e: AXUIElement, _ cmd: [String: Any]) throws {
    guard let text = cmd["text"] as? String, !text.isEmpty else { throw Fail(message: "`text` is required") }
    guard let value = attr(e, "AXValue") as? String else { throw Fail(message: "the element has no text value") }
    let (prefix, suffix) = (cmd["prefix"] as? String ?? "", cmd["suffix"] as? String ?? "")
    let haystack = value as NSString
    var matches: [Int] = []
    var from = 0
    while from < haystack.length {
        let r = haystack.range(of: prefix + text + suffix, range: NSRange(location: from, length: haystack.length - from))
        if r.location == NSNotFound { break }
        matches.append(r.location + (prefix as NSString).length)
        from = r.location + 1
    }
    guard matches.count == 1, let start = matches.first else {
        throw Fail(message: matches.isEmpty ? "text not found in the element" : "the text appears \(matches.count) times; add prefix or suffix")
    }
    let length = (text as NSString).length
    var range = switch cmd["selection_type"] as? String ?? "text" {
    case "cursor_before": CFRange(location: start, length: 0)
    case "cursor_after": CFRange(location: start + length, length: 0)
    default: CFRange(location: start, length: length)
    }
    let result = AXUIElementSetAttributeValue(e, "AXSelectedTextRange" as CFString, AXValueCreate(.cfRange, &range)!)
    guard result == .success else { throw Fail(message: "the selection cannot be set (AXError \(result.rawValue))") }
}

func act(_ cmd: [String: Any]) throws -> [String: Any] {
    guard let action = cmd["action"] as? String else { throw Fail(message: "`action` is required") }
    if action == "list_apps" { return ["text": listApps()] }
    guard AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": true] as CFDictionary) else {
        throw Fail(message: "Accessibility permission is missing. Allow GreenCode (or the terminal running it) in System Settings > Privacy & Security > Accessibility, then retry.")
    }
    guard let name = cmd["app"] as? String else { throw Fail(message: "`app` is required") }
    let app = try find(name)
    let pid = app.processIdentifier
    let hold = cmd["duration"] as? Double ?? 0
    guard (0...10).contains(hold) else { throw Fail(message: "`duration` must be 0–10 seconds") }
    let frontmost = NSWorkspace.shared.frontmostApplication?.processIdentifier
    var note: String?
    switch action {
    case "get_app_state": break
    case "click":
        let count = cmd["click_count"] as? Int ?? 1
        guard (1...3).contains(count), hold * Double(count) <= 10 else {
            throw Fail(message: "`click_count` must be 1–3 and total hold time at most 10 seconds")
        }
        let button = cmd["mouse_button"] as? String ?? "left"
        guard ["left", "right", "middle"].contains(button) else { throw Fail(message: "`mouse_button` must be left, right or middle") }
        let mods = try flags((cmd["key"] as? String ?? "").split(separator: "+").map(String.init))
        // AXPress needs no pointer at all; fall back to posted mouse events for everything else.
        if button == "left", count == 1, mods.isEmpty, hold == 0, cmd["element_index"] != nil,
            AXUIElementPerformAction(try element(cmd, pid), "AXPress" as CFString) == .success { break }
        let window = try pointerTarget(pid)
        let at = try point(cmd, pid, window)
        let (down, up, mb): (NSEvent.EventType, NSEvent.EventType, CGMouseButton) = switch button {
        case "right": (.rightMouseDown, .rightMouseUp, .right)
        case "middle": (.otherMouseDown, .otherMouseUp, .center)
        default: (.leftMouseDown, .leftMouseUp, .left)
        }
        for i in 1...count {
            let number = Int.random(in: 1...Int(Int32.max))
            try mouse(down, at, mb, window, pid, number: number, click: i, flags: mods)
            if hold > 0 { usleep(useconds_t(hold * 1_000_000)) }
            try mouse(up, at, mb, window, pid, number: number, click: i, flags: mods)
        }
    case "perform_secondary_action":
        guard let wanted = cmd["secondary_action"] as? String else { throw Fail(message: "`secondary_action` is required") }
        let e = try element(cmd, pid)
        let squash = { (s: String) in s.replacingOccurrences(of: " ", with: "").lowercased() }
        let actions = secondary(e)
        guard let ax = actions.first(where: { squash($0) == squash(wanted) || squash(display($0)) == squash(wanted) }) else {
            throw Fail(message: "the element does not expose `\(wanted)`; it has: \(actions.map(display).joined(separator: ", "))")
        }
        let result = AXUIElementPerformAction(e, ax as CFString)
        guard result == .success else { throw Fail(message: "\(ax) failed (AXError \(result.rawValue))") }
    case "set_value":
        guard let value = cmd["value"] as? String else { throw Fail(message: "`value` is required") }
        let result = AXUIElementSetAttributeValue(try element(cmd, pid), "AXValue" as CFString, value as CFString)
        guard result == .success else { throw Fail(message: "the element's value cannot be set (AXError \(result.rawValue)); click it and use type_text") }
    case "select_text": try selectText(try element(cmd, pid), cmd)
    case "type_text":
        guard let text = cmd["text"] as? String else { throw Fail(message: "`text` is required") }
        type(text, pid)
    case "paste":
        guard let text = cmd["text"] as? String else { throw Fail(message: "`text` is required") }
        note = try paste(text, cmd["format"] as? String ?? "text", pid)
    case "press_key":
        guard let combo = cmd["key"] as? String else { throw Fail(message: "`key` is required") }
        note = try pressKey(combo, pid, hold: hold)
    case "scroll":
        let direction = cmd["direction"] as? String ?? "down"
        guard ["up", "down", "left", "right"].contains(direction) else { throw Fail(message: "`direction` must be up, down, left or right") }
        let pages = cmd["pages"] as? Double ?? 1
        guard (0...100).contains(pages) else { throw Fail(message: "`pages` must be 0–100") }
        let vertical = direction == "up" || direction == "down"
        let sign: Double = direction == "down" || direction == "right" ? 1 : -1
        let element = try cmd["element_index"] == nil ? nil : element(cmd, pid)
        if let element, scrollAX(element, vertical: vertical, by: sign * pages) { break }
        let window = try pointerTarget(pid)
        let at = element != nil || cmd["x"] != nil || cmd["y"] != nil
            ? try point(cmd, pid, window) : CGPoint(x: window.bounds.midX, y: window.bounds.midY)
        let size = element.flatMap(frame)?.size ?? window.bounds.size
        // A page is 80% of the scrolled view; content moves the way `direction` names.
        let total = Int32(-sign * (vertical ? size.height : size.width) * 0.8 * pages)
        for tick in 0..<10 {
            let step = total * Int32(tick + 1) / 10 - total * Int32(tick) / 10
            if step == 0 { continue }
            guard let ev = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 2, wheel1: vertical ? step : 0, wheel2: vertical ? 0 : step, wheel3: 0) else { throw Fail(message: "cannot construct a window wheel event") }
            routePointer(ev, at, window, pid)
            usleep(15_000)
        }
    case "drag":
        let window = try pointerTarget(pid)
        let (from, to) = (try point(cmd, pid, window, "from_x", "from_y"), try point(cmd, pid, window, "to_x", "to_y"))
        let number = Int.random(in: 1...Int(Int32.max))
        try mouse(.leftMouseDown, from, .left, window, pid, number: number)
        for step in 1...10 {
            let t = CGFloat(step) / 10
            try mouse(.leftMouseDragged, CGPoint(x: from.x + (to.x - from.x) * t, y: from.y + (to.y - from.y) * t), .left, window, pid, number: number)
        }
        try mouse(.leftMouseUp, to, .left, window, pid, number: number)
    default: throw Fail(message: "unknown action `\(action)`")
    }
    var reply = try state(app, settle: action != "get_app_state", full: cmd["disable_diff"] as? Bool == true)
    if frontmost != pid, NSWorkspace.shared.frontmostApplication?.processIdentifier == pid {
        let focus = "(the target app became frontmost during this action)"
        note = note.map { "\(focus)\n\($0)" } ?? focus
    }
    if let note { reply["text"] = "\(note)\n\(reply["text"] ?? "")" }
    return reply
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
