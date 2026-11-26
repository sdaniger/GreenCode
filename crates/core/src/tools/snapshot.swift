// GreenCode SnapShot helper: a global shortcut that captures the frontmost window.
//   snapshot check|request <screen|accessibility|input>...  -> JSON array of permissions still missing
//   snapshot run <shift|ctrl-opt-cmd-s|ctrl-opt-cmd-g> <0|1 include app text> <dir>
// `run` writes <dir>/snap-<ms>.png, then <dir>/snap-<ms>.json ({app,title,text}) as the "complete" marker,
// and prints "capture" (or "error <message>") per shortcut press. It exits when stdin closes.
import AppKit
import ApplicationServices
import Carbon.HIToolbox

func granted(_ name: String, prompt: Bool) -> Bool {
    switch name {
    case "screen": return CGPreflightScreenCaptureAccess() || (prompt && CGRequestScreenCaptureAccess())
    case "accessibility":
        return AXIsProcessTrustedWithOptions(prompt ? [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary : nil)
    case "input": return CGPreflightListenEventAccess() || (prompt && CGRequestListenEventAccess())
    default: return false
    }
}

func say(_ line: String) {
    FileHandle.standardOutput.write((line.replacingOccurrences(of: "\n", with: " ") + "\n").data(using: .utf8)!)
}

func attr(_ e: AXUIElement, _ name: String) -> AnyObject? {
    var v: AnyObject?
    return AXUIElementCopyAttributeValue(e, name as CFString, &v) == .success ? v : nil
}

// Titles, descriptions and values of the window's controls, in tree order; bounded by count, size and time.
func appText(_ pid: pid_t) -> String {
    guard AXIsProcessTrusted() else { return "" }
    let ax = AXUIElementCreateApplication(pid)
    AXUIElementSetMessagingTimeout(ax, 1)
    AXUIElementSetAttributeValue(ax, "AXManualAccessibility" as CFString, kCFBooleanTrue)  // Chromium/Electron build their tree on request
    guard let root = attr(ax, "AXFocusedWindow") ?? attr(ax, "AXMainWindow"), CFGetTypeID(root) == AXUIElementGetTypeID() else { return "" }
    let deadline = Date().addingTimeInterval(2)
    var lines: [String] = [], seen = 0, size = 0
    func walk(_ e: AXUIElement, _ depth: Int) {
        guard depth < 12, seen < 600, size < 6000, Date() < deadline else { return }
        seen += 1
        for name in ["AXTitle", "AXDescription", "AXValue"] {
            if let s = attr(e, name) as? String, !s.isEmpty, s != lines.last {
                lines.append(String(s.prefix(500)))
                size += min(s.count, 500)
            }
        }
        for child in attr(e, "AXChildren") as? [AXUIElement] ?? [] { walk(child, depth + 1) }
    }
    walk(root as! AXUIElement, 0)
    return lines.joined(separator: "\n")
}

func capture(dir: String, includeText: Bool) {
    guard let app = NSWorkspace.shared.frontmostApplication else { return say("error 前面のアプリが見つかりません") }
    let pid = app.processIdentifier
    let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    // The list is ordered front to back: the first normal window of the frontmost app is the one in use.
    guard let win = windows.first(where: {
        guard $0[kCGWindowOwnerPID as String] as? pid_t == pid, $0[kCGWindowLayer as String] as? Int == 0,
            let b = $0[kCGWindowBounds as String] as? NSDictionary, let r = CGRect(dictionaryRepresentation: b) else { return false }
        return r.width >= 50 && r.height >= 50
    }), let id = win[kCGWindowNumber as String] as? Int else { return say("error \(app.localizedName ?? "アプリ") に撮影できるウィンドウがありません") }
    let stem = dir + "/snap-\(Int(Date().timeIntervalSince1970 * 1000))"
    let shot = Process()
    shot.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
    shot.arguments = ["-x", "-o", "-l\(id)", stem + ".png.tmp"]
    shot.standardError = FileHandle.nullDevice
    guard (try? shot.run()) != nil else { return say("error screencapture を起動できません") }
    shot.waitUntilExit()
    guard FileManager.default.fileExists(atPath: stem + ".png.tmp") else { return say("error 画面収録の許可が必要です") }
    let meta: [String: String] = [
        "app": app.localizedName ?? "", "title": win[kCGWindowName as String] as? String ?? "",
        "text": includeText ? appText(pid) : "",
    ]
    do {
        try FileManager.default.moveItem(atPath: stem + ".png.tmp", toPath: stem + ".png")
        try JSONSerialization.data(withJSONObject: meta).write(to: URL(fileURLWithPath: stem + ".json.tmp"))
        try FileManager.default.moveItem(atPath: stem + ".json.tmp", toPath: stem + ".json")
        say("capture")
    } catch { say("error \(error.localizedDescription)") }
}

let args = CommandLine.arguments
guard args.count >= 3 else { say("usage"); exit(2) }
if args[1] == "check" || args[1] == "request" {
    let missing = args[2...].filter { !granted($0, prompt: args[1] == "request") }
    say(String(data: try! JSONSerialization.data(withJSONObject: Array(missing)), encoding: .utf8)!)
    exit(0)
}
guard args[1] == "run", args.count == 5 else { say("usage"); exit(2) }
let (hotkey, includeText, dir) = (args[2], args[3] == "1", args[4])
try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
let queue = DispatchQueue(label: "snapshot")
let fire = { queue.async { capture(dir: dir, includeText: includeText) } }

// The app exits with its parent: stdin closing is the stop signal. Let a capture already running finish so its
// marker survives a quit; only then exit.
Thread.detachNewThread { while FileHandle.standardInput.availableData.count > 0 {}; queue.sync {}; exit(0) }

let app = NSApplication.shared
app.setActivationPolicy(.accessory)

// Both Shift keys pressed together (left = 56, right = 60), with no other modifier; fires once per press.
// A listen-only CGEventTap only needs Input Monitoring; NSEvent key monitors would also need Accessibility.
var shiftDown = Set<Int64>(), shiftArmed = true, shiftTap: CFMachPort?
var fireCapture: (() -> Void)?
func shiftTapCallback(_: CGEventTapProxy, _ type: CGEventType, _ event: CGEvent, _: UnsafeMutableRawPointer?) -> Unmanaged<CGEvent>? {
    if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
        if let tap = shiftTap { CGEvent.tapEnable(tap: tap, enable: true) }
        return nil
    }
    let code = event.getIntegerValueField(.keyboardEventKeycode)
    guard code == 56 || code == 60 else { return Unmanaged.passUnretained(event) }
    // Flags only say that *a* Shift is held, so the key's own presence in `shiftDown` tells press from release.
    if shiftDown.contains(code) || !event.flags.contains(.maskShift) {
        shiftDown.remove(code)
    } else {
        shiftDown.insert(code)
    }
    let others = event.flags.intersection([.maskCommand, .maskAlternate, .maskControl])
    if shiftDown.count == 2 && others.isEmpty && shiftArmed { shiftArmed = false; fireCapture?() }
    if shiftDown.isEmpty { shiftArmed = true }
    return Unmanaged.passUnretained(event)
}

if hotkey == "shift" {
    fireCapture = fire
    let mask = CGEventMask(1 << CGEventType.flagsChanged.rawValue)
    guard let tap = CGEvent.tapCreate(tap: .cgSessionEventTap, place: .headInsertEventTap, options: .listenOnly,
        eventsOfInterest: mask, callback: shiftTapCallback, userInfo: nil) else {
        say("error 入力監視の許可が必要です"); exit(1)
    }
    shiftTap = tap
    CFRunLoopAddSource(CFRunLoopGetMain(), CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0), .commonModes)
    CGEvent.tapEnable(tap: tap, enable: true)
} else {
    let code: UInt32 = hotkey == "ctrl-opt-cmd-g" ? 5 : 1  // kVK_ANSI_G / kVK_ANSI_S
    var ref: EventHotKeyRef?
    var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
    InstallEventHandler(GetApplicationEventTarget(), { _, _, _ in fire(); return noErr }, 1, &spec, nil, nil)
    guard RegisterEventHotKey(code, UInt32(controlKey | optionKey | cmdKey), EventHotKeyID(signature: 0x47_43_53_53, id: 1), GetApplicationEventTarget(), 0, &ref) == noErr
    else { say("error ショートカットを登録できません"); exit(1) }
}
app.run()
