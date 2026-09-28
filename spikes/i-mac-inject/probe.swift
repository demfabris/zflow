// Spike I probe: can the logged-in app post input with CGEventPost well
// enough to be a zflow receiver? README.md says how to run it and RESULT.md
// records the answer. Throwaway code: the evidence matters, not this file.
//
// Safety rules this file keeps:
// - Every posted event carries MARK in kCGEventSourceUserData, so the taps
//   and the window log can tell posted input from the owner's hands.
// - A click or scroll goes out only after a hit test finds the probe window
//   under that point. A key goes out only while the probe window is key.
// - Held buttons, keys, modifiers and the Caps Lock state are tracked and
//   put back when a test ends, when the 60 s watchdog fires, on SIGINT,
//   SIGTERM and SIGHUP, and (best effort) when the process crashes.

import AppKit
import ApplicationServices
import Carbon
import Darwin
import IOKit
import IOKit.hidsystem
import IOKit.pwr_mgt

let MARK: Int64 = 0x7A_666C_6F77 // "zflow" in ASCII
let WATCHDOG_SECONDS = 60.0
let ALL_TESTS = ["suppress", "delta", "click", "flags", "iso", "caps", "scroll", "keys"]
let OPT_IN_TESTS = ["media", "wake"]
let KNOWN_TESTS = ALL_TESTS + OPT_IN_TESTS + ["feel"]

// Mac virtual keycodes. The numbers match src/macos/mod.rs:867-988.
enum Key {
    static let a: CGKeyCode = 0
    static let l: CGKeyCode = 37
    static let k: CGKeyCode = 40
    // Positions as mod.rs names them after capture_bridge.c's ISO swap. An ISO
    // keyboard itself reports 10 for the key left of 1 and 50 for left of Z.
    static let leftOfOne: CGKeyCode = 50 // HID 0x35
    static let leftOfZ: CGKeyCode = 10 // HID 0x64
    static let leftCommand: CGKeyCode = 55
    static let capsLock: CGKeyCode = 57
    static let f13: CGKeyCode = 105 // stands in for PrintScreen
    static let f14: CGKeyCode = 107 // stands in for ScrollLock
    static let f15: CGKeyCode = 113 // stands in for Pause
}

let deviceLeftCommand: UInt64 = 0x08 // NX_DEVICELCMDKEYMASK

// NX_KEYTYPE_* from IOKit's ev_keymap.h.
enum MediaKey {
    static let soundUp = 0
    static let soundDown = 1
    static let brightnessUp = 2
    static let brightnessDown = 3
}

let permitAll: CGEventFilterMask = [
    .permitLocalMouseEvents, .permitLocalKeyboardEvents, .permitSystemDefinedEvents,
]

// MARK: - Output

enum Out {
    static let lock = NSLock()

    static func line(_ text: String) {
        lock.lock()
        defer { lock.unlock() }
        fputs(text + "\n", stdout)
        fflush(stdout)
    }
}

func show(_ value: Any) -> String {
    switch value {
    case let b as Bool: return b ? "1" : "0"
    case let d as Double: return String(format: "%.3f", d)
    case let f as CGFloat: return String(format: "%.3f", Double(f))
    case let p as CGPoint: return String(format: "%.1f,%.1f", Double(p.x), Double(p.y))
    case let s as String:
        if s.isEmpty { return "-" }
        let flat = s.replacingOccurrences(of: "\n", with: " | ")
        return flat.contains(" ") ? "\"\(flat)\"" : flat
    case let a as [Any]:
        return a.isEmpty ? "-" : a.map(show).joined(separator: ",")
    default: return "\(value)"
    }
}

func hex<T: BinaryInteger>(_ value: T) -> String {
    "0x" + String(value, radix: 16)
}

func report(_ test: String, _ pairs: KeyValuePairs<String, Any>) {
    Out.line("TEST \(test) " + pairs.map { "\($0.key)=\(show($0.value))" }.joined(separator: " "))
}

enum Verdicts {
    static let lock = NSLock()
    static var lines: [String] = []
}

func verdict(_ check: String, _ result: String, _ note: String = "") {
    let text = "VERDICT \(check) \(result)" + (note.isEmpty ? "" : " " + note)
    Out.line(text)
    Verdicts.lock.lock()
    Verdicts.lines.append(text)
    Verdicts.lock.unlock()
}

func pass(_ ok: Bool) -> String { ok ? "PASS" : "FAIL" }

func scalars(_ s: String) -> String {
    s.isEmpty ? "-" : s.unicodeScalars.map { String(format: "U+%04X", $0.value) }.joined(separator: "+")
}

// The string itself when every character prints on one line, else "-".
func printable(_ s: String) -> String {
    let ok = !s.isEmpty && s.unicodeScalars.allSatisfy { $0.value > 32 && $0.value < 0xF700 && $0.value != 127 }
    return ok ? s : "-"
}

struct Abort: Error, CustomStringConvertible {
    let description: String
    init(_ description: String) { self.description = description }
}

// MARK: - Time and threads

func now() -> Double { ProcessInfo.processInfo.systemUptime }

func nap(_ seconds: Double) {
    if seconds > 0 { Thread.sleep(forTimeInterval: seconds) }
}

func onMain<T>(_ body: () -> T) -> T {
    if Thread.isMainThread { return body() }
    return DispatchQueue.main.sync(execute: body)
}

// MARK: - Environment

func sysctlString(_ name: String) -> String {
    var size = 0
    guard sysctlbyname(name, nil, &size, nil, 0) == 0, size > 0 else { return "?" }
    var buffer = [CChar](repeating: 0, count: size)
    guard sysctlbyname(name, &buffer, &size, nil, 0) == 0 else { return "?" }
    return String(decoding: buffer.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
}

func fourcc(_ value: UInt32) -> String {
    let bytes = [24, 16, 8, 0].map { UInt8((value >> UInt32($0)) & 0xFF) }
    guard bytes.allSatisfy({ $0 >= 32 && $0 < 127 }) else { return hex(value) }
    return String(decoding: bytes, as: UTF8.self).trimmingCharacters(in: .whitespaces)
}

// "ANSI", "ISO", "JIS" or "????" for a kCGKeyboardEventKeyboardType value.
func layoutName(_ keyboardType: Int) -> String {
    fourcc(KBGetLayoutType(Int16(truncatingIfNeeded: keyboardType)))
}

func keyboardTypeTable() -> [String: [Int]] {
    var table: [String: [Int]] = [:]
    for type in 0...255 { table[layoutName(type), default: []].append(type) }
    return table
}

func ranges(_ values: [Int]) -> String {
    guard var start = values.first else { return "-" }
    var previous = start
    var parts: [String] = []
    for value in values.dropFirst() {
        if value == previous + 1 { previous = value; continue }
        parts.append(start == previous ? "\(start)" : "\(start)-\(previous)")
        start = value
        previous = value
    }
    parts.append(start == previous ? "\(start)" : "\(start)-\(previous)")
    return parts.joined(separator: ",")
}

func inputSourceID() -> String {
    guard let source = TISCopyCurrentKeyboardLayoutInputSource()?.takeRetainedValue(),
          let raw = TISGetInputSourceProperty(source, kTISPropertyInputSourceID)
    else { return "?" }
    return Unmanaged<CFString>.fromOpaque(raw).takeUnretainedValue() as String
}

// System Settings stores both in units of 15 ms.
func globalPreference(_ key: String) -> Int? {
    (CFPreferencesCopyAppValue(key as CFString, kCFPreferencesAnyApplication) as? NSNumber)?.intValue
}

func registryValue(_ entry: io_registry_entry_t, _ key: String) -> Any? {
    IORegistryEntryCreateCFProperty(entry, key as CFString, kCFAllocatorDefault, 0)?.takeRetainedValue()
}

// Each keyboard's own layout from IOHIDFamily's StandardType (0 ANSI, 1 ISO,
// 2 JIS). Unlike LMGetKbdType, this does not depend on the last key pressed.
func keyboardsReport() {
    var iterator: io_iterator_t = 0
    guard IOServiceGetMatchingServices(kIOMainPortDefault, IOServiceMatching("AppleHIDKeyboardEventDriverV2"),
                                       &iterator) == KERN_SUCCESS else { return }
    defer { IOObjectRelease(iterator) }
    var index = 0
    while true {
        let service = IOIteratorNext(iterator)
        if service == 0 { break }
        defer { IOObjectRelease(service) }
        let standard = (registryValue(service, "StandardType") as? NSNumber)?.intValue ?? -1
        let layouts = ["ANSI", "ISO", "JIS"]
        report("env", [
            "keyboard": index,
            "product": registryValue(service, "Product") as? String ?? "",
            "builtin": (registryValue(service, "Built-In") as? NSNumber)?.boolValue ?? false,
            "transport": registryValue(service, "Transport") as? String ?? "",
            "vendor_id": (registryValue(service, "VendorID") as? NSNumber)?.intValue ?? -1,
            "product_id": (registryValue(service, "ProductID") as? NSNumber)?.intValue ?? -1,
            "standard_type": standard,
            "standard_layout": layouts.indices.contains(standard) ? layouts[standard] : "?",
            "language": registryValue(service, "KeyboardLanguage") as? String ?? "",
            "country_code": (registryValue(service, "CountryCode") as? NSNumber)?.intValue ?? -1,
        ])
        index += 1
    }
}

func displayIDs() -> [CGDirectDisplayID] {
    var ids = [CGDirectDisplayID](repeating: 0, count: 32)
    var count: UInt32 = 0
    guard CGGetActiveDisplayList(32, &ids, &count) == .success else { return [] }
    return Array(ids.prefix(Int(count)))
}

func displays() -> [CGRect] { displayIDs().map { CGDisplayBounds($0) } }

func rectText(_ r: CGRect) -> String {
    String(format: "%.0f,%.0f,%.0fx%.0f", Double(r.minX), Double(r.minY), Double(r.width), Double(r.height))
}

func environmentReport(_ context: String) {
    let v = ProcessInfo.processInfo.operatingSystemVersion
    report("env", [
        "context": context,
        "date": ISO8601DateFormatter().string(from: Date()),
        "macos": "\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)",
        "build": sysctlString("kern.osversion"),
        "model": sysctlString("hw.model"),
        "cpu": sysctlString("machdep.cpu.brand_string"),
        "bundle": Bundle.main.bundleIdentifier ?? "none",
        "pid": Int(getpid()),
    ])
    report("env", [
        "ax_trusted": AXIsProcessTrusted(),
        "post_access": CGPreflightPostEventAccess(),
        "listen_access": CGPreflightListenEventAccess(),
        "secure_input": IsSecureEventInputEnabled(),
    ])
    let keyboard = Int(LMGetKbdType())
    let table = keyboardTypeTable()
    report("env", [
        "kbtype": keyboard,
        "layout": layoutName(keyboard),
        "input_source": onMain { inputSourceID() },
        "caps": Caps.cgState(),
        "hid_flags": hex(CGEventSource.flagsState(.hidSystemState).rawValue),
    ])
    keyboardsReport()
    report("env", [
        "kbtypes_ansi": ranges(table["ANSI"] ?? []),
        "kbtypes_iso": ranges(table["ISO"] ?? []),
        "kbtypes_jis": ranges(table["JIS"] ?? []),
        "type40": layoutName(40), "type41": layoutName(41), "type42": layoutName(42),
    ])
    let (delay, interval, doubleClick) = onMain {
        (NSEvent.keyRepeatDelay, NSEvent.keyRepeatInterval, NSEvent.doubleClickInterval)
    }
    report("env", [
        "initial_key_repeat": globalPreference("InitialKeyRepeat").map { "\($0)" } ?? "unset",
        "key_repeat": globalPreference("KeyRepeat").map { "\($0)" } ?? "unset",
        "appkit_delay_s": delay,
        "appkit_interval_s": interval,
        "double_click_s": doubleClick,
    ])
    report("env", [
        "combined_suppression_s": CGEventSource(stateID: .combinedSessionState)?.localEventsSuppressionInterval ?? -1,
        "hid_suppression_s": CGEventSource(stateID: .hidSystemState)?.localEventsSuppressionInterval ?? -1,
        "system_click_counter": Clicks.systemCounter(),
    ])
    for (index, id) in displayIDs().enumerated() {
        report("env", [
            "display": index,
            "id": Int(id),
            "builtin": CGDisplayIsBuiltin(id) != 0,
            "main": CGDisplayIsMain(id) != 0,
            "bounds": rectText(CGDisplayBounds(id)),
            "asleep": CGDisplayIsAsleep(id) != 0,
        ])
    }
    report("env", ["cursor": cursor()])
}

// MARK: - Geometry

func cursor() -> CGPoint { CGEvent(source: nil)?.location ?? .zero }

func primaryHeight() -> CGFloat { CGDisplayBounds(CGMainDisplayID()).height }

func onDisplay(_ p: CGPoint) -> Bool { displays().contains { $0.contains(p) } }

func displayUnion() -> CGRect { displays().reduce(CGRect.null) { $0.union($1) } }

// Keeps a point on some display: unchanged when it is on one, else moved to
// the nearest point of the nearest display. Returns whether it moved.
func snap(_ p: CGPoint) -> (CGPoint, Bool) {
    let rects = displays()
    if rects.contains(where: { $0.contains(p) }) { return (p, false) }
    var best = p
    var bestDistance = CGFloat.infinity
    for r in rects {
        let q = CGPoint(x: min(max(p.x, r.minX), r.maxX - 1), y: min(max(p.y, r.minY), r.maxY - 1))
        let distance = hypot(q.x - p.x, q.y - p.y)
        if distance < bestDistance { best = q; bestDistance = distance }
    }
    return (best, true)
}

func targetCenter() -> CGPoint {
    onMain {
        guard let view = UI.view, let window = view.window else { return .zero }
        let r = window.convertToScreen(view.convert(view.bounds, to: nil))
        return CGPoint(x: r.midX, y: primaryHeight() - r.midY)
    }
}

func titleBarPoint() -> CGPoint {
    onMain {
        let f = UI.window?.frame ?? .zero
        return CGPoint(x: f.midX, y: primaryHeight() - (f.maxY - 12))
    }
}

// True when a mouse-down at this point would hit the probe window.
func probeWindowAt(_ p: CGPoint) -> Bool {
    onMain {
        let cocoa = NSPoint(x: p.x, y: primaryHeight() - p.y)
        let number = NSWindow.windowNumber(at: cocoa, belowWindowWithWindowNumber: 0)
        return number == UI.window?.windowNumber
    }
}

func probeFront() -> Bool {
    onMain { NSApp.isActive && (UI.window?.isKeyWindow ?? false) }
}

func guardKeys() throws {
    guard probeFront() else { throw Abort("the probe window is not key; refusing to post a key") }
}

func guardPoint(_ p: CGPoint) throws {
    guard probeWindowAt(p) else {
        throw Abort("\(show(p)) is not over the probe window; refusing to click or scroll there")
    }
}

func ensureFront(_ why: String) throws {
    if probeFront() { return }
    onMain {
        NSApp.activate()
        UI.window?.makeKeyAndOrderFront(nil)
    }
    let start = now()
    while now() - start < 1.5 {
        if probeFront() { return }
        nap(0.05)
    }
    // A posted click on the window body activates it on macOS 27 (click test).
    let center = targetCenter()
    if probeWindowAt(center), let source = try? receiverSource(),
       (try? click(center, source, state: 1, number: Clicks.next())) != nil {
        nap(0.6)
        if probeFront() { return }
    }
    say("Click inside this window to bring it to the front (\(why)). Waiting 20 s.")
    let wait = now()
    while now() - wait < 20 {
        if probeFront() { return }
        nap(0.1)
    }
    throw Abort("the probe window never became key (\(why))")
}

// Gets focus back without waiting for a person: activate, then a posted
// title-bar click on the probe window, which deskflow#9852 says still works.
func tryFront(_ source: CGEventSource) -> String? {
    if probeFront() { return "already" }
    onMain { NSApp.activate() }
    nap(1)
    if probeFront() { return "activate" }
    let title = titleBarPoint()
    guard probeWindowAt(title), (try? click(title, source, state: 1, number: Clicks.next())) != nil else { return nil }
    nap(0.6)
    return probeFront() ? "titlebar_click" : nil
}

// MARK: - Posting

enum Clicks {
    static var number: Int64 = 0

    // Deskflow's rule for macOS 27: start just past the system's own count of
    // button presses, add one per press, and reuse it for the release and drags.
    static func next() -> Int64 {
        if number == 0 { number = systemCounter() }
        number += 1
        return number
    }

    static func systemCounter() -> Int64 {
        [CGEventType.leftMouseDown, .rightMouseDown, .otherMouseDown].reduce(Int64(0)) {
            $0 + Int64(CGEventSource.counterForEventType(.hidSystemState, eventType: $1))
        }
    }
}

func made(_ event: CGEvent?, _ what: String) throws -> CGEvent {
    guard let event else { throw Abort("could not create \(what)") }
    return event
}

func post(_ event: CGEvent, at tap: CGEventTapLocation = .cghidEventTap) {
    event.setIntegerValueField(.eventSourceUserData, value: MARK)
    Safety.track(event)
    event.post(tap: tap)
}

// The source ROADMAP section 4 plans for the receiver.
func receiverSource() throws -> CGEventSource {
    guard let source = CGEventSource(stateID: .hidSystemState) else {
        throw Abort("could not create an HID system-state event source")
    }
    source.localEventsSuppressionInterval = 0
    source.setLocalEventsFilterDuringSuppressionState(permitAll, state: .eventSuppressionStateSuppressionInterval)
    source.setLocalEventsFilterDuringSuppressionState(permitAll, state: .eventSuppressionStateRemoteMouseDrag)
    source.userData = MARK
    return source
}

func mouseEvent(_ type: CGEventType, _ p: CGPoint, _ source: CGEventSource?,
                button: CGMouseButton = .left) throws -> CGEvent {
    try made(CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: p, mouseButton: button),
             "a mouse event")
}

func moveEvent(to p: CGPoint, delta: CGPoint, _ source: CGEventSource?) throws -> CGEvent {
    let event = try mouseEvent(.mouseMoved, p, source)
    event.setIntegerValueField(.mouseEventDeltaX, value: Int64(delta.x.rounded()))
    event.setIntegerValueField(.mouseEventDeltaY, value: Int64(delta.y.rounded()))
    event.setDoubleValueField(.mouseEventDeltaX, value: Double(delta.x))
    event.setDoubleValueField(.mouseEventDeltaY, value: Double(delta.y))
    return event
}

func moveTo(_ p: CGPoint, _ source: CGEventSource?) throws {
    let c = cursor()
    post(try moveEvent(to: p, delta: CGPoint(x: p.x - c.x, y: p.y - c.y), source))
}

// Posts a left click after the hit test passes. Returns the flags the down
// event was created with, before any override.
@discardableResult
func click(_ p: CGPoint, _ source: CGEventSource?, state: Int64?, number: Int64?,
           flags: CGEventFlags? = nil) throws -> CGEventFlags {
    let down = try mouseEvent(.leftMouseDown, p, source)
    let up = try mouseEvent(.leftMouseUp, p, source)
    let created = down.flags
    for event in [down, up] {
        if let state { event.setIntegerValueField(.mouseEventClickState, value: state) }
        if let number { event.setIntegerValueField(.mouseEventNumber, value: number) }
        if let flags { event.flags = flags }
    }
    try guardPoint(p)
    post(down)
    nap(0.03)
    post(up)
    return created
}

func keyEvent(_ code: CGKeyCode, down: Bool, _ source: CGEventSource?) throws -> CGEvent {
    try made(CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down), "a key event")
}

func flagsEvent(_ code: CGKeyCode, _ flags: CGEventFlags, _ source: CGEventSource?) throws -> CGEvent {
    let event = try keyEvent(code, down: true, source)
    event.type = .flagsChanged
    event.flags = flags
    return event
}

// Posts a key press and release while the probe window is key. Returns the
// flags the down event was created with, before any override.
@discardableResult
func tapKey(_ code: CGKeyCode, _ source: CGEventSource?, flags: CGEventFlags? = nil,
            keyboardType: Int64? = nil, hold: Double = 0.03) throws -> CGEventFlags {
    let down = try keyEvent(code, down: true, source)
    let up = try keyEvent(code, down: false, source)
    let created = down.flags
    for event in [down, up] {
        if let flags { event.flags = flags }
        if let keyboardType { event.setIntegerValueField(.keyboardEventKeyboardType, value: keyboardType) }
    }
    try guardKeys()
    post(down)
    nap(hold)
    post(up)
    return created
}

// MARK: - Safety

enum Safety {
    static let lock = NSLock()
    static var buttons = Set<Int64>()
    static var keys = Set<CGKeyCode>()
    static var modifiers = Set<CGKeyCode>()

    static func modifierMask(_ code: CGKeyCode) -> CGEventFlags? {
        switch code {
        case 54, 55: return .maskCommand
        case 56, 60: return .maskShift
        case 58, 61: return .maskAlternate
        case 59, 62: return .maskControl
        case 63: return .maskSecondaryFn
        default: return nil
        }
    }

    static func track(_ event: CGEvent) {
        lock.lock()
        defer { lock.unlock() }
        let button = event.getIntegerValueField(.mouseEventButtonNumber)
        let code = CGKeyCode(truncatingIfNeeded: event.getIntegerValueField(.keyboardEventKeycode))
        switch event.type {
        case .leftMouseDown, .rightMouseDown, .otherMouseDown: buttons.insert(button)
        case .leftMouseUp, .rightMouseUp, .otherMouseUp: buttons.remove(button)
        case .keyDown: keys.insert(code)
        case .keyUp: keys.remove(code)
        case .flagsChanged:
            guard let mask = modifierMask(code) else { break }
            if event.flags.contains(mask) { modifiers.insert(code) } else { modifiers.remove(code) }
        default: break
        }
    }

    static func isHeld(modifier code: CGKeyCode) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return modifiers.contains(code)
    }

    // Releases everything still held and puts Caps Lock back. The crash path
    // skips a busy lock and writes without the output lock.
    static func releaseAll(crashing: Bool = false) {
        if crashing {
            guard lock.try() else { return }
        } else {
            lock.lock()
        }
        let heldButtons = buttons, heldKeys = keys, heldModifiers = modifiers
        buttons.removeAll()
        keys.removeAll()
        modifiers.removeAll()
        lock.unlock()

        let source = CGEventSource(stateID: .hidSystemState)
        let p = cursor()
        for button in heldButtons {
            let type: CGEventType = button == 0 ? .leftMouseUp : button == 1 ? .rightMouseUp : .otherMouseUp
            let mouseButton = CGMouseButton(rawValue: UInt32(truncatingIfNeeded: button)) ?? .left
            if let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: p,
                                   mouseButton: mouseButton) {
                event.setIntegerValueField(.eventSourceUserData, value: MARK)
                event.post(tap: .cghidEventTap)
            }
        }
        for code in heldKeys {
            if let event = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: false) {
                event.setIntegerValueField(.eventSourceUserData, value: MARK)
                event.post(tap: .cghidEventTap)
            }
        }
        for code in heldModifiers {
            if let event = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: false) {
                event.type = .flagsChanged
                event.flags = []
                event.setIntegerValueField(.eventSourceUserData, value: MARK)
                event.post(tap: .cghidEventTap)
            }
        }
        if !(heldButtons.isEmpty && heldKeys.isEmpty && heldModifiers.isEmpty) {
            let text = "RELEASE buttons=\(show(heldButtons.map { Int($0) })) keys=\(show(heldKeys.map { Int($0) })) "
                + "modifiers=\(show(heldModifiers.map { Int($0) }))"
            if crashing { fputs(text + "\n", stdout); fflush(stdout) } else { Out.line(text) }
        }
        let restored = Caps.restore()
        if restored != "none" && !crashing { Out.line("RESTORE caps=\(restored)") }
    }
}

enum Caps {
    static var original: Bool? // Caps flag in the HID system state before the test
    static var originalIOHID: Bool? // IOHIDSystem lock state before the test
    static var connect: io_connect_t = 0
    static var openResult: kern_return_t = -1

    static func cgState() -> Bool {
        CGEventSource.flagsState(.hidSystemState).contains(.maskAlphaShift)
    }

    static func open() {
        guard connect == 0 else { return }
        let service = IOServiceGetMatchingService(kIOMainPortDefault, IOServiceMatching(kIOHIDSystemClass))
        guard service != 0 else { openResult = -2; return }
        defer { IOObjectRelease(service) }
        var handle: io_connect_t = 0
        openResult = IOServiceOpen(service, mach_task_self_, UInt32(kIOHIDParamConnectType), &handle)
        if openResult == KERN_SUCCESS { connect = handle }
    }

    static func ioState() -> Bool? {
        guard connect != 0 else { return nil }
        var state = false
        return IOHIDGetModifierLockState(connect, Int32(kIOHIDCapsLockState), &state) == KERN_SUCCESS ? state : nil
    }

    static func ioSet(_ on: Bool) -> kern_return_t {
        IOHIDSetModifierLockState(connect, Int32(kIOHIDCapsLockState), on)
    }

    static func remember() {
        original = cgState()
        originalIOHID = ioState()
    }

    // Puts the lock back the way the test found it. IOHIDSystem holds the real
    // lock, so it gets the last word. Returns what it did.
    static func restore() -> String {
        guard let want = original else { return "none" }
        var steps: [String] = []
        for _ in 0..<2 {
            if let wantIO = originalIOHID, let current = ioState(), current != wantIO {
                _ = ioSet(wantIO)
                steps.append("iohid")
                usleep(250_000)
            }
            if cgState() != want {
                let source = CGEventSource(stateID: .hidSystemState)
                if let event = CGEvent(keyboardEventSource: source, virtualKey: Key.capsLock, keyDown: true) {
                    event.type = .flagsChanged
                    event.flags = want ? .maskAlphaShift : []
                    event.setIntegerValueField(.eventSourceUserData, value: MARK)
                    event.post(tap: .cghidEventTap)
                }
                steps.append("flagschanged57")
                usleep(250_000)
            }
        }
        return steps.isEmpty ? "unchanged" : steps.joined(separator: ",")
    }

    static func close() {
        if connect != 0 { IOServiceClose(connect) }
        connect = 0
        original = nil
        originalIOHID = nil
    }
}

enum Watchdog {
    static let queue = DispatchQueue(label: "dev.zflow.spike.inject-probe.watchdog")
    static var timer: DispatchSourceTimer?

    static func arm(_ test: String) {
        queue.sync {
            timer?.cancel()
            let t = DispatchSource.makeTimerSource(queue: queue)
            t.schedule(deadline: .now() + WATCHDOG_SECONDS)
            t.setEventHandler {
                Out.line("WATCHDOG \(test) ran past \(Int(WATCHDOG_SECONDS)) s; releasing everything and exiting")
                Safety.releaseAll()
                verdict(test, "FAIL", "watchdog fired")
                exit(3)
            }
            t.resume()
            timer = t
        }
    }

    static func disarm() {
        queue.sync {
            timer?.cancel()
            timer = nil
        }
    }
}

enum Signals {
    static var sources: [DispatchSourceSignal] = []

    static func install() {
        for number in [SIGINT, SIGTERM, SIGHUP] {
            signal(number, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: number, queue: Watchdog.queue)
            source.setEventHandler {
                Out.line("SIGNAL \(number); releasing everything and exiting")
                Safety.releaseAll()
                exit(128 + number)
            }
            source.resume()
            sources.append(source)
        }
        for number in [SIGABRT, SIGSEGV, SIGBUS, SIGILL, SIGTRAP] {
            signal(number) { caught in
                Safety.releaseAll(crashing: true)
                signal(caught, SIG_DFL)
                raise(caught)
            }
        }
    }
}

// MARK: - What the window receives

struct Seen {
    var type: NSEvent.EventType
    var marked = false
    var clickCount = -1
    var eventNumber = -1
    var cgClickState: Int64 = -1
    var cgEventNumber: Int64 = -1
    var flags: UInt = 0
    var keyCode = -1
    var chars = ""
    var charsIgnoring = ""
    var isRepeat = false
    var keyboardType: Int64 = -1
    var dx = 0.0
    var dy = 0.0
    var scrollX = 0.0
    var scrollY = 0.0
    var precise = false
    var phase: UInt = 0
    var momentumPhase: UInt = 0
    var inverted = false
    var appActive = false
    var windowKey = false

    var hasCommand: Bool { flags & NSEvent.ModifierFlags.command.rawValue != 0 }
}

let buttonTypes: Set<NSEvent.EventType> = [
    .leftMouseDown, .leftMouseUp, .rightMouseDown, .rightMouseUp, .otherMouseDown, .otherMouseUp,
    .leftMouseDragged, .rightMouseDragged, .otherMouseDragged,
]
let moveTypes: Set<NSEvent.EventType> = [.mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged]

func typeName(_ type: NSEvent.EventType) -> String {
    switch type {
    case .leftMouseDown: return "leftMouseDown"
    case .leftMouseUp: return "leftMouseUp"
    case .rightMouseDown: return "rightMouseDown"
    case .rightMouseUp: return "rightMouseUp"
    case .otherMouseDown: return "otherMouseDown"
    case .otherMouseUp: return "otherMouseUp"
    case .mouseMoved: return "mouseMoved"
    case .leftMouseDragged: return "leftMouseDragged"
    case .rightMouseDragged: return "rightMouseDragged"
    case .otherMouseDragged: return "otherMouseDragged"
    case .mouseEntered: return "mouseEntered"
    case .mouseExited: return "mouseExited"
    case .keyDown: return "keyDown"
    case .keyUp: return "keyUp"
    case .flagsChanged: return "flagsChanged"
    case .appKitDefined: return "appKitDefined"
    case .systemDefined: return "systemDefined"
    case .scrollWheel: return "scrollWheel"
    default: return "type\(type.rawValue)"
    }
}

enum Log {
    static let lock = NSLock()
    static var seen: [Seen] = []
    static var quietMoves = false

    static func mark() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return seen.count
    }

    static func since(_ index: Int) -> [Seen] {
        lock.lock()
        defer { lock.unlock() }
        return index < seen.count ? Array(seen[index...]) : []
    }

    static func setQuiet(_ quiet: Bool) {
        lock.lock()
        quietMoves = quiet
        lock.unlock()
    }

    // Runs on the main thread for every event the app receives.
    static func record(_ event: NSEvent) {
        let type = event.type
        let cg = event.cgEvent
        var s = Seen(type: type)
        s.marked = cg?.getIntegerValueField(.eventSourceUserData) == MARK
        s.flags = event.modifierFlags.rawValue
        s.appActive = NSApp.isActive
        s.windowKey = UI.window?.isKeyWindow ?? false
        var extra: [String] = []
        if buttonTypes.contains(type) {
            s.clickCount = event.clickCount
            s.eventNumber = event.eventNumber
            s.cgClickState = cg?.getIntegerValueField(.mouseEventClickState) ?? -1
            s.cgEventNumber = cg?.getIntegerValueField(.mouseEventNumber) ?? -1
            extra += ["cc=\(s.clickCount)", "cg_cs=\(s.cgClickState)", "num=\(s.eventNumber)",
                      "cg_num=\(s.cgEventNumber)", "btn=\(event.buttonNumber)"]
        }
        if moveTypes.contains(type) || type == .scrollWheel {
            s.dx = Double(event.deltaX)
            s.dy = Double(event.deltaY)
            extra += ["dx=\(show(s.dx))", "dy=\(show(s.dy))"]
        }
        if type == .keyDown || type == .keyUp || type == .flagsChanged {
            s.keyCode = Int(event.keyCode)
            s.keyboardType = cg?.getIntegerValueField(.keyboardEventKeyboardType) ?? -1
            extra += ["kc=\(s.keyCode)", "kbtype=\(s.keyboardType)"]
        }
        if type == .keyDown || type == .keyUp {
            s.chars = event.characters ?? ""
            s.charsIgnoring = event.charactersIgnoringModifiers ?? ""
            s.isRepeat = event.isARepeat
            extra += ["chars=\(scalars(s.chars))", "text=\(printable(s.chars))", "ign=\(scalars(s.charsIgnoring))",
                      "rep=\(s.isRepeat ? 1 : 0)"]
        }
        if type == .scrollWheel {
            s.scrollX = Double(event.scrollingDeltaX)
            s.scrollY = Double(event.scrollingDeltaY)
            s.precise = event.hasPreciseScrollingDeltas
            s.phase = event.phase.rawValue
            s.momentumPhase = event.momentumPhase.rawValue
            s.inverted = event.isDirectionInvertedFromDevice
            extra += ["sdx=\(show(s.scrollX))", "sdy=\(show(s.scrollY))", "precise=\(s.precise ? 1 : 0)",
                      "phase=\(s.phase)", "mphase=\(s.momentumPhase)", "inverted=\(s.inverted ? 1 : 0)",
                      "cg_continuous=\(cg?.getIntegerValueField(.scrollWheelEventIsContinuous) ?? -1)"]
        }
        if type == .systemDefined {
            extra += ["subtype=\(event.subtype.rawValue)", "data1=\(hex(event.data1))"]
        }
        lock.lock()
        seen.append(s)
        let quiet = quietMoves && type == .mouseMoved
        lock.unlock()
        if quiet { return }
        let text = "type=\(typeName(type)) mark=\(s.marked ? 1 : 0) active=\(s.appActive ? 1 : 0) "
            + "key=\(s.windowKey ? 1 : 0) flags=\(hex(s.flags)) " + extra.joined(separator: " ")
        Out.line("EVENT t=\(String(format: "%.3f", event.timestamp)) " + text)
        if let view = UI.view {
            view.lines.append(text)
            if view.lines.count > 200 { view.lines.removeFirst(100) }
            view.needsDisplay = true
        }
    }
}

// MARK: - Window

final class TargetView: NSView {
    var banner = "zflow inject probe"
    var lines: [String] = []

    override var acceptsFirstResponder: Bool { true }
    override var isFlipped: Bool { true }

    // The local monitor already logged these; swallowing them stops the beep.
    override func keyDown(with event: NSEvent) {}
    override func keyUp(with event: NSEvent) {}
    override func flagsChanged(with event: NSEvent) {}

    override func draw(_ dirtyRect: NSRect) {
        NSColor.textBackgroundColor.setFill()
        bounds.fill()
        let ring = NSBezierPath(ovalIn: NSRect(x: bounds.midX - 24, y: bounds.midY - 24, width: 48, height: 48))
        NSColor.systemRed.withAlphaComponent(0.25).setFill()
        ring.fill()
        let big: [NSAttributedString.Key: Any] = [
            .font: NSFont.boldSystemFont(ofSize: 22), .foregroundColor: NSColor.labelColor,
        ]
        (banner as NSString).draw(in: NSRect(x: 16, y: 12, width: bounds.width - 32, height: 100), withAttributes: big)
        let small: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedSystemFont(ofSize: 11, weight: .regular),
            .foregroundColor: NSColor.secondaryLabelColor,
        ]
        var y: CGFloat = 120
        for line in lines.suffix(34) {
            (line as NSString).draw(at: NSPoint(x: 16, y: y), withAttributes: small)
            y += 14
        }
    }
}

enum UI {
    static var window: NSWindow?
    static var view: TargetView?
}

func say(_ text: String) {
    Out.line("SAY \(text)")
    DispatchQueue.main.async {
        UI.view?.banner = text
        UI.view?.needsDisplay = true
    }
}

// MARK: - Test 1: suppress

struct MoveCounts {
    var hidLocal = 0
    var hidPosted = 0
    var sessionLocal = 0
    var sessionPosted = 0
    var sessionLocalPath = 0
}

enum Taps {
    static let lock = NSLock()
    static var counts = MoveCounts()
    static var ports: [CFMachPort] = []
    static var sources: [CFRunLoopSource] = []

    static func count(_ which: Int, _ event: CGEvent) {
        let marked = event.getIntegerValueField(.eventSourceUserData) == MARK
        let path = abs(event.getIntegerValueField(.mouseEventDeltaX)) + abs(event.getIntegerValueField(.mouseEventDeltaY))
        lock.lock()
        defer { lock.unlock() }
        switch (which, marked) {
        case (1, false): counts.hidLocal += 1
        case (1, true): counts.hidPosted += 1
        case (2, false):
            counts.sessionLocal += 1
            counts.sessionLocalPath += Int(path)
        case (2, true): counts.sessionPosted += 1
        default: break
        }
    }

    static func reset() {
        lock.lock()
        counts = MoveCounts()
        lock.unlock()
    }

    static func snapshot() -> MoveCounts {
        lock.lock()
        defer { lock.unlock() }
        return counts
    }

    // Main thread only. Returns an error, or nil once both taps run.
    static func start() -> String? {
        let mask = [CGEventType.mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged]
            .reduce(CGEventMask(0)) { $0 | (CGEventMask(1) << CGEventMask($1.rawValue)) }
        for (which, location) in [(1, CGEventTapLocation.cghidEventTap), (2, CGEventTapLocation.cgSessionEventTap)] {
            guard let port = CGEvent.tapCreate(tap: location, place: .headInsertEventTap, options: .listenOnly,
                                               eventsOfInterest: mask, callback: tapCallback,
                                               userInfo: UnsafeMutableRawPointer(bitPattern: which)),
                  let source = CFMachPortCreateRunLoopSource(nil, port, 0)
            else {
                stop()
                return "could not create a listen-only tap (\(which == 1 ? "hid" : "session"))"
            }
            CFRunLoopAddSource(CFRunLoopGetMain(), source, .commonModes)
            CGEvent.tapEnable(tap: port, enable: true)
            ports.append(port)
            sources.append(source)
        }
        return nil
    }

    static func stop() {
        for port in ports {
            CGEvent.tapEnable(tap: port, enable: false)
            CFMachPortInvalidate(port)
        }
        for source in sources { CFRunLoopRemoveSource(CFRunLoopGetMain(), source, .commonModes) }
        ports.removeAll()
        sources.removeAll()
    }

    static func reenable() {
        for port in ports { CGEvent.tapEnable(tap: port, enable: true) }
    }
}

let tapCallback: CGEventTapCallBack = { _, type, event, refcon in
    if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
        Taps.reenable()
    } else {
        Taps.count(Int(bitPattern: refcon), event)
    }
    return Unmanaged.passUnretained(event)
}

// The process-wide calls Deskflow makes. Deprecated since 10.6, so they are
// looked up at runtime instead of linked.
func setGlobalSuppression() -> String {
    typealias SetInterval = @convention(c) (Double) -> Int32
    typealias SetFilter = @convention(c) (UInt32, UInt32) -> Int32
    let everywhere = UnsafeMutableRawPointer(bitPattern: -2) // RTLD_DEFAULT
    guard let interval = dlsym(everywhere, "CGSetLocalEventsSuppressionInterval"),
          let filter = dlsym(everywhere, "CGSetLocalEventsFilterDuringSuppressionState")
    else { return "missing" }
    let setInterval = unsafeBitCast(interval, to: SetInterval.self)
    let setFilter = unsafeBitCast(filter, to: SetFilter.self)
    let results = [setInterval(0), setFilter(permitAll.rawValue, 0), setFilter(permitAll.rawValue, 1)]
    return results.map(String.init).joined(separator: ",")
}

func testSuppress() throws {
    if let error = onMain({ Taps.start() }) {
        verdict("suppress", "FAIL", error + "; grant Input Monitoring to the probe and rerun")
        return
    }
    defer { onMain { Taps.stop() } }
    Log.setQuiet(true)
    defer { Log.setQuiet(false) }

    let hid = CGEventSource(stateID: .hidSystemState)
    report("suppress", [
        "default_interval_s": CGEventSource(stateID: .combinedSessionState)?.localEventsSuppressionInterval ?? -1,
        "hid_interval_s": hid?.localEventsSuppressionInterval ?? -1,
        "hid_filter_interval": hex(hid?.getLocalEventsFilterDuringSuppressionState(
            .eventSuppressionStateSuppressionInterval).rawValue ?? 0),
        "hid_filter_drag": hex(hid?.getLocalEventsFilterDuringSuppressionState(
            .eventSuppressionStateRemoteMouseDrag).rawValue ?? 0),
    ])
    let permit = try receiverSource()
    // global_permit runs last: its process-wide setting may outlive the arm.
    let arms: [(name: String, source: CGEventSource?, posts: Bool)] = [
        ("baseline", nil, false),
        ("default_source", nil, true),
        ("hid_permit", permit, true),
        ("global_permit", nil, true),
    ]
    var results: [String: MoveCounts] = [:]
    for (index, arm) in arms.enumerated() {
        if arm.name == "global_permit" {
            report("suppress", ["arm": arm.name, "global_calls": setGlobalSuppression()])
        }
        for left in [2, 1] {
            say("Arm \(index + 1)/4 (\(arm.name)) starts in \(left) s. Circle slowly on the trackpad now and keep going.")
            nap(1)
        }
        say("Arm \(index + 1)/4 (\(arm.name)): keep circling for 8 s")
        let appMark = Log.mark()
        Taps.reset()
        let start = now()
        var posted = 0
        var sign: CGFloat = 1
        if arm.posts {
            while now() - start < 8 {
                let c = cursor()
                post(try moveEvent(to: CGPoint(x: c.x + sign, y: c.y), delta: CGPoint(x: sign, y: 0), arm.source))
                posted += 1
                sign = -sign
                nap(start + Double(posted) * 0.008 - now())
            }
        } else {
            nap(8)
        }
        let counts = Taps.snapshot()
        let appMoves = Log.since(appMark).filter { $0.type == .mouseMoved }
        results[arm.name] = counts
        report("suppress", [
            "arm": arm.name,
            "posted": posted,
            "hid_local": counts.hidLocal,
            "hid_posted": counts.hidPosted,
            "session_local": counts.sessionLocal,
            "session_posted": counts.sessionPosted,
            "session_local_path_px": counts.sessionLocalPath,
            "app_local": appMoves.filter { !$0.marked }.count,
            "app_posted": appMoves.filter { $0.marked }.count,
        ])
        say("Stop. Rest for a second.")
        nap(1)
    }
    let base = results["baseline"]?.sessionLocal ?? 0
    for name in ["default_source", "hid_permit", "global_permit"] {
        let counts = results[name] ?? MoveCounts()
        report("suppress", [
            "arm": name,
            "local_vs_baseline": Double(counts.sessionLocal) / Double(max(base, 1)),
            "hid_local_vs_baseline": Double(counts.hidLocal) / Double(max(results["baseline"]?.hidLocal ?? 0, 1)),
        ])
    }
    for name in ["hid_permit", "global_permit"] {
        let kept = Double(results[name]?.sessionLocal ?? 0) / Double(max(base, 1))
        if base < 100 {
            verdict("suppress.\(name)", "MANUAL", "baseline saw only \(base) local moves; move more and rerun")
        } else {
            verdict("suppress.\(name)", pass(kept >= 0.5),
                    "local moves kept \(Int(kept * 100))% of baseline (pass needs 50%)")
        }
    }
    verdict("suppress.feel", "MANUAL", "did the cursor keep following your finger in arms 3 and 4? did it stall in arm 2?")
}

// MARK: - Test 2: delta

func testDelta() throws {
    let source = try receiverSource()
    say("delta: hands off the trackpad and mouse for 5 s")
    nap(1.5)
    let center = targetCenter()
    try moveTo(center, source)
    nap(0.1)

    let p0 = cursor()
    let sent1 = CGPoint(x: p0.x + 10, y: p0.y)
    post(try moveEvent(to: sent1, delta: CGPoint(x: 10, y: 0), source))
    nap(0.08)
    let p1 = cursor()
    report("delta", ["step": "location_and_delta", "from": p0, "sent_location": sent1, "sent_delta": "10,0", "cursor": p1])

    post(try moveEvent(to: p1, delta: CGPoint(x: 10, y: 0), source))
    nap(0.08)
    let p2 = cursor()
    let moved = p2.x - p1.x
    let driver = abs(moved) < 0.5 ? "location" : abs(moved - 10) < 0.5 ? "delta" : "other"
    report("delta", ["step": "delta_only", "from": p1, "sent_location": p1, "sent_delta": "10,0", "cursor": p2,
                     "driver": driver])

    let off = CGPoint(x: displayUnion().maxX + 1000, y: p2.y)
    post(try moveEvent(to: off, delta: CGPoint(x: off.x - p2.x, y: 0), source))
    nap(0.08)
    let p3 = cursor()
    report("delta", ["step": "offscreen", "sent_location": off, "cursor": p3, "clamped_by_macos": onDisplay(p3)])

    try moveTo(center, source)
    nap(0.1)
    let p4 = cursor()
    post(try moveEvent(to: CGPoint(x: p4.x + 1, y: p4.y), delta: CGPoint(x: 1, y: 0), source))
    nap(0.08)
    let p5 = cursor()
    report("delta", ["step": "one_point", "from": p4, "cursor": p5, "moved": p5.x - p4.x])

    let p6 = cursor()
    for i in 1...20 {
        post(try moveEvent(to: CGPoint(x: p6.x + CGFloat(i), y: p6.y), delta: CGPoint(x: 1, y: 0), source))
        nap(0.008)
    }
    nap(0.08)
    let p7 = cursor()
    report("delta", ["step": "fast_location_series", "sent_points": 20, "moved": p7.x - p6.x])

    let p8 = cursor()
    for _ in 1...20 {
        post(try moveEvent(to: cursor(), delta: CGPoint(x: 5, y: 0), source))
        nap(0.008)
    }
    nap(0.08)
    let p9 = cursor()
    report("delta", ["step": "fast_delta_series", "sent_delta_total": 100, "moved": p9.x - p8.x])

    try moveTo(center, source)
    verdict("delta.location_drives", pass(driver == "location"), "the cursor followed the \(driver) field")
    let one = p5.x - p4.x, twenty = p7.x - p6.x
    verdict("delta.no_acceleration", pass(abs(one - 1) < 0.5 && abs(twenty - 20) < 0.5),
            "1 point moved \(show(one)), 20 fast points moved \(show(twenty))")
}

// MARK: - Test 3: click

func testClick() throws {
    try ensureFront("click")
    let source = try receiverSource()
    let center = targetCenter()
    try moveTo(center, source)
    nap(0.1)
    let interval = onMain { NSEvent.doubleClickInterval }
    let gap = interval + 0.3
    let sample = try mouseEvent(.leftMouseDown, center, source)
    report("click", [
        "default_click_state": sample.getIntegerValueField(.mouseEventClickState),
        "default_event_number": sample.getIntegerValueField(.mouseEventNumber),
        "system_counter": Clicks.systemCounter(),
        "double_click_s": interval,
    ])

    let arms: [(name: String, clicks: Int, state: Bool, number: Bool)] = [
        ("single", 1, true, true),
        ("double_state_number", 2, true, true),
        ("double_state_nonumber", 2, true, false),
        ("double_nostate_number", 2, false, true),
        ("double_nostate_nonumber", 2, false, false),
    ]
    var downCounts: [String: [Int]] = [:]
    for arm in arms {
        try ensureFront("click \(arm.name)")
        let mark = Log.mark()
        for k in 1...arm.clicks {
            try click(center, source, state: arm.state ? Int64(k) : nil, number: arm.number ? Clicks.next() : nil)
            nap(0.06)
        }
        nap(0.25)
        let seen = Log.since(mark)
        let downs = seen.filter { $0.type == .leftMouseDown }
        let ups = seen.filter { $0.type == .leftMouseUp }
        downCounts[arm.name] = downs.map(\.clickCount)
        report("click", [
            "arm": arm.name,
            "downs": downs.count,
            "ups": ups.count,
            "marked": downs.filter(\.marked).count,
            "down_click_counts": downs.map(\.clickCount),
            "up_click_counts": ups.map(\.clickCount),
            "appkit_numbers": downs.map(\.eventNumber),
            "cg_numbers": downs.map(\.cgEventNumber),
        ])
        nap(gap)
    }
    verdict("click.single", pass(downCounts["single"] == [1]), "click counts \(show(downCounts["single"] ?? []))")
    verdict("click.double", pass(downCounts["double_state_number"] == [1, 2]),
            "click counts \(show(downCounts["double_state_number"] ?? []))")

    try ensureFront("drag")
    let mark = Log.mark()
    let number = Clicks.next()
    try guardPoint(center)
    let down = try mouseEvent(.leftMouseDown, center, source)
    down.setIntegerValueField(.mouseEventClickState, value: 1)
    down.setIntegerValueField(.mouseEventNumber, value: number)
    post(down)
    var last = center
    for i in 1...10 {
        last = CGPoint(x: center.x + CGFloat(5 * i), y: center.y)
        let drag = try mouseEvent(.leftMouseDragged, last, source)
        drag.setIntegerValueField(.mouseEventClickState, value: 1)
        drag.setIntegerValueField(.mouseEventNumber, value: number)
        drag.setIntegerValueField(.mouseEventDeltaX, value: 5)
        post(drag)
        nap(0.016)
    }
    let up = try mouseEvent(.leftMouseUp, last, source)
    up.setIntegerValueField(.mouseEventClickState, value: 1)
    up.setIntegerValueField(.mouseEventNumber, value: number)
    post(up)
    nap(0.25)
    let seen = Log.since(mark)
    let dragged = seen.filter { $0.type == .leftMouseDragged }.count
    let ups = seen.filter { $0.type == .leftMouseUp }.count
    report("click", ["arm": "drag", "sent_drags": 10, "dragged_seen": dragged, "ups": ups])
    verdict("click.drag", pass(dragged >= 1 && ups == 1), "\(dragged) of 10 drag events arrived")
    nap(gap)

    try backgroundFocus(center, source, gap: gap)
}

// Accessibility raise of our own window, the workaround in deskflow#9852.
func axRaiseSelf() -> String {
    let app = AXUIElementCreateApplication(getpid())
    var value: CFTypeRef?
    let copied = AXUIElementCopyAttributeValue(app, kAXMainWindowAttribute as CFString, &value)
    var raised = AXError.failure
    if copied == .success, let value, CFGetTypeID(value) == AXUIElementGetTypeID() {
        raised = AXUIElementPerformAction(unsafeBitCast(value, to: AXUIElement.self), kAXRaiseAction as CFString)
    }
    let front = AXUIElementSetAttributeValue(app, kAXFrontmostAttribute as CFString, kCFBooleanTrue)
    return "copy:\(copied.rawValue),raise:\(raised.rawValue),frontmost:\(front.rawValue)"
}

func backgroundFocus(_ center: CGPoint, _ source: CGEventSource, gap: Double) throws {
    guard let finder = NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.finder").first else {
        verdict("click.background_focus", "MANUAL", "Finder is not running, so nothing could take focus")
        return
    }
    let title = titleBarPoint()
    let arms: [(name: String, point: CGPoint, number: String, axRaise: Bool)] = [
        ("body_nonumber", center, "none", false),
        ("body_number", center, "counter", false),
        ("body_lownumber", center, "low", false),
        ("titlebar_number", title, "counter", false),
        // An Accessibility raise of our own window from this thread trapped in
        // AppKit (makeKeyAndOrderFront off the main thread), so it is left out.
    ]
    var activated: [String] = []
    try ensureFront("background arms")
    for arm in arms {
        guard let how = tryFront(source) else {
            report("click", ["arm": "background_" + arm.name, "skipped": "could not get focus back after the previous arm"])
            break
        }
        report("click", ["arm": "background_" + arm.name, "focus_back_by": how])
        onMain {
            NSApp.yieldActivation(to: finder)
            _ = finder.activate(from: NSRunningApplication.current, options: [])
        }
        nap(0.6)
        let before = onMain { (NSApp.isActive, finder.isActive) }
        if before.0 || !before.1 {
            report("click", ["arm": "background_" + arm.name, "finder_took_focus": false])
            continue
        }
        guard probeWindowAt(arm.point) else {
            report("click", ["arm": "background_" + arm.name, "covered": true,
                             "note": "a Finder window covers the probe; move it away and rerun"])
            continue
        }
        var ax = "-"
        var activeAfterAX = false
        if arm.axRaise {
            ax = axRaiseSelf()
            nap(0.3)
            activeAfterAX = onMain { NSApp.isActive }
        }
        let mark = Log.mark()
        let number: Int64? = arm.number == "counter" ? Clicks.next() : arm.number == "low" ? 1 : nil
        try click(arm.point, source, state: 1, number: number)
        nap(0.6)
        let after = onMain { (NSApp.isActive, UI.window?.isKeyWindow ?? false) }
        let downs = Log.since(mark).filter { $0.type == .leftMouseDown }
        report("click", [
            "arm": "background_" + arm.name,
            "finder_active_before": before.1,
            "ax": ax,
            "active_after_ax": activeAfterAX,
            "probe_active_after": after.0,
            "probe_key_after": after.1,
            "down_delivered": downs.count,
            "sent_number": number.map { "\($0)" } ?? "unset",
        ])
        if after.0 { activated.append(arm.name) }
        nap(gap)
    }
    try ensureFront("after the background arms")
    verdict("click.background_focus", activated.isEmpty ? "FAIL" : "PASS",
            "arms that activated the probe: \(activated.isEmpty ? "none" : activated.joined(separator: ","))")
}

// MARK: - Test 4: flags

func testFlags() throws {
    try ensureFront("flags")
    let source = try receiverSource()
    let center = targetCenter()
    try moveTo(center, source)
    nap(0.1)
    let gap = onMain { NSEvent.doubleClickInterval } + 0.3
    let command = CGEventFlags(rawValue: CGEventFlags.maskCommand.rawValue | deviceLeftCommand)
    defer {
        if Safety.isHeld(modifier: Key.leftCommand), let up = try? flagsEvent(Key.leftCommand, [], source) {
            post(up)
        }
    }
    func states() -> (String, String) {
        (hex(CGEventSource.flagsState(.hidSystemState).rawValue),
         hex(CGEventSource.flagsState(.combinedSessionState).rawValue))
    }

    try guardKeys()
    var mark = Log.mark()
    post(try flagsEvent(Key.leftCommand, command, source))
    nap(0.15)
    let downSeen = Log.since(mark).filter { $0.type == .flagsChanged }
    var (hidState, sessionState) = states()
    report("flags", ["step": "command_down", "sent_flags": hex(command.rawValue), "seen": downSeen.count,
                     "seen_flags": downSeen.map { hex($0.flags) }, "hid_state": hidState,
                     "session_state": sessionState])

    mark = Log.mark()
    try click(center, source, state: 1, number: Clicks.next(), flags: command)
    nap(0.2)
    let clickWith = Log.since(mark).first { $0.type == .leftMouseDown }
    report("flags", ["step": "click_with_flags", "sent_flags": hex(command.rawValue),
                     "seen_flags": clickWith.map { hex($0.flags) } ?? "none",
                     "command": clickWith?.hasCommand ?? false])
    nap(gap)

    mark = Log.mark()
    let createdClick = try click(center, source, state: 1, number: Clicks.next())
    nap(0.2)
    let clickWithout = Log.since(mark).first { $0.type == .leftMouseDown }
    report("flags", ["step": "click_without_flags", "created_flags": hex(createdClick.rawValue),
                     "seen_flags": clickWithout.map { hex($0.flags) } ?? "none",
                     "command": clickWithout?.hasCommand ?? false])
    nap(gap)

    mark = Log.mark()
    try tapKey(Key.a, source, flags: command)
    nap(0.2)
    let keyWith = Log.since(mark).first { $0.type == .keyDown }
    report("flags", ["step": "key_with_flags", "keycode": Int(Key.a),
                     "seen_flags": keyWith.map { hex($0.flags) } ?? "none",
                     "command": keyWith?.hasCommand ?? false,
                     "chars": scalars(keyWith?.chars ?? ""), "ign": scalars(keyWith?.charsIgnoring ?? "")])

    mark = Log.mark()
    let createdKey = try tapKey(Key.a, source)
    nap(0.2)
    let keyWithout = Log.since(mark).first { $0.type == .keyDown }
    report("flags", ["step": "key_without_flags", "created_flags": hex(createdKey.rawValue),
                     "seen_flags": keyWithout.map { hex($0.flags) } ?? "none",
                     "command": keyWithout?.hasCommand ?? false,
                     "chars": scalars(keyWithout?.chars ?? "")])

    mark = Log.mark()
    post(try flagsEvent(Key.leftCommand, [], source))
    nap(0.15)
    let upSeen = Log.since(mark).filter { $0.type == .flagsChanged }
    (hidState, sessionState) = states()
    let released = CGEventSource.flagsState(.hidSystemState).contains(.maskCommand) == false
    report("flags", ["step": "command_up", "seen": upSeen.count, "seen_flags": upSeen.map { hex($0.flags) },
                     "hid_state": hidState, "session_state": sessionState])

    verdict("flags.click_with_flags", pass(clickWith?.hasCommand ?? false), "Cmd on a clicked mouseDown")
    verdict("flags.key_with_flags", pass(keyWith?.hasCommand ?? false), "Cmd on a keyDown")
    verdict("flags.command_released", pass(released), "HID system state after the Cmd release")
    report("flags", ["held_command_merged_into_click": clickWithout?.hasCommand ?? false,
                     "held_command_merged_into_key": keyWithout?.hasCommand ?? false])
}

// MARK: - Test 5: iso

func testISO() throws {
    try ensureFront("iso")
    let source = try receiverSource()
    let table = onMain { keyboardTypeTable() }
    let real = onMain { Int(LMGetKbdType()) }
    report("iso", ["real_kbtype": real, "real_layout": onMain { layoutName(real) },
                   "input_source": onMain { inputSourceID() }])
    report("iso", ["ansi_types": ranges(table["ANSI"] ?? []), "iso_types": ranges(table["ISO"] ?? []),
                   "jis_types": ranges(table["JIS"] ?? [])])
    func pick(_ layout: String, _ preferred: Int) -> Int? {
        onMain { layoutName(preferred) } == layout ? preferred : table[layout]?.first
    }
    var chars: [String: String] = [:]
    var sent = 0, delivered = 0
    for (name, type) in [("ansi", pick("ANSI", 40)), ("iso", pick("ISO", 41)), ("jis", pick("JIS", 42))] {
        guard let type else {
            report("iso", ["layout": name, "kbtype": "none"])
            continue
        }
        for code in [Key.leftOfZ, Key.leftOfOne] {
            let mark = Log.mark()
            try tapKey(code, source, keyboardType: Int64(type))
            nap(0.15)
            sent += 1
            let down = Log.since(mark).first { $0.type == .keyDown }
            if down != nil { delivered += 1 }
            chars["\(name)/\(code)"] = down?.chars ?? ""
            report("iso", [
                "layout": name, "kbtype": type, "keycode": Int(code),
                "seen_keycode": down?.keyCode ?? -1, "seen_kbtype": down?.keyboardType ?? -1,
                "chars": scalars(down?.chars ?? ""), "text": printable(down?.chars ?? ""),
            ])
        }
    }
    let a10 = chars["ansi/10"] ?? "", a50 = chars["ansi/50"] ?? ""
    let i10 = chars["iso/10"] ?? "", i50 = chars["iso/50"] ?? ""
    report("iso", ["type_changes_chars": a10 != i10 || a50 != i50,
                   "swapped_by_type": a10 == i50 && a50 == i10 && a10 != a50])
    verdict("iso.delivered", pass(sent > 0 && delivered == sent), "\(delivered) of \(sent) keys arrived")

    let prompts = [
        ("left_of_1", "Press the key left of 1 once, on the built-in keyboard if the lid is open (8 s)"),
        ("left_of_z", "Press the key between left Shift and Z once, if that keyboard has one (8 s)"),
    ]
    for (label, prompt) in prompts {
        say(prompt)
        let mark = Log.mark()
        let start = now()
        var hit: Seen?
        while now() - start < 8 {
            hit = Log.since(mark).first { $0.type == .keyDown }
            if hit != nil { break }
            nap(0.05)
        }
        let type = Int(hit?.keyboardType ?? -1)
        report("iso", [
            "real_key": label, "pressed": hit != nil, "keycode": hit?.keyCode ?? -1, "kbtype": type,
            "layout": type >= 0 ? onMain { layoutName(type) } : "-",
            "chars": scalars(hit?.chars ?? ""), "text": printable(hit?.chars ?? ""),
        ])
        nap(0.3)
    }
    let after = onMain { Int(LMGetKbdType()) }
    let layout = onMain { layoutName(after) }
    report("iso", ["real_kbtype_after": after, "layout": layout])
    verdict("iso.real_keyboard", "MANUAL", "the last keyboard used reports \(layout); compare the chars above with the keycaps")
}

// MARK: - Test 6: caps

func testCaps() throws {
    try ensureFront("caps")
    let source = try receiverSource()
    Caps.open()
    Caps.remember()
    defer {
        let how = Caps.restore()
        report("caps", ["final_restore": how, "cg_final": Caps.cgState(),
                        "iohid_final": Caps.ioState().map { $0 ? "1" : "0" } ?? "-",
                        "restored": Caps.cgState() == (Caps.original ?? false)])
        Caps.close()
    }
    let original = Caps.original ?? false
    let originalIO = Caps.originalIOHID
    report("caps", ["iohid_open": hex(UInt32(bitPattern: Caps.openResult)), "cg_before": original,
                    "iohid_before": originalIO.map { $0 ? "1" : "0" } ?? "-"])
    say("caps: watch the Caps Lock light on the built-in keyboard")
    nap(1.5)

    var worked: [String] = []
    for method in ["flagschanged57", "keydown57", "iohid"] {
        let want = !original
        let mark = Log.mark()
        var result = "-"
        switch method {
        case "flagschanged57":
            try guardKeys()
            let flags: CGEventFlags = want ? .maskAlphaShift : []
            post(try flagsEvent(Key.capsLock, flags, source))
            nap(0.05)
            post(try flagsEvent(Key.capsLock, flags, source))
        case "keydown57":
            try tapKey(Key.capsLock, source)
        default:
            guard Caps.connect != 0 else {
                report("caps", ["method": method, "skipped": "no IOHIDSystem connection"])
                continue
            }
            result = hex(UInt32(bitPattern: Caps.ioSet(want)))
        }
        nap(0.4)
        let cg = Caps.cgState()
        let io = Caps.ioState()
        let typedMark = Log.mark()
        let created = try tapKey(Key.a, source)
        nap(0.15)
        let typed = Log.since(typedMark).first { $0.type == .keyDown }?.chars ?? ""
        let flagEvents = Log.since(mark).filter { $0.type == .flagsChanged }.map { hex($0.flags) }
        let toggled = io.map { $0 != (originalIO ?? original) } ?? (cg != original)
        report("caps", [
            "method": method, "result": result, "cg_after": cg,
            "iohid_after": io.map { $0 ? "1" : "0" } ?? "-", "toggled": toggled,
            "typed": scalars(typed), "text": printable(typed), "a_created_flags": hex(created.rawValue),
            "seen_flags_changed": flagEvents,
        ])
        if toggled { worked.append(method) }
        let how = Caps.restore()
        report("caps", ["method": method, "restore": how, "cg_now": Caps.cgState(),
                        "iohid_now": Caps.ioState().map { $0 ? "1" : "0" } ?? "-"])
        nap(0.6)
    }
    verdict("caps.toggle", worked.isEmpty ? "FAIL" : "PASS",
            "methods that toggled the lock: \(worked.isEmpty ? "none" : worked.joined(separator: ","))")
    verdict("caps.light", "MANUAL", "did the Caps Lock light follow each method that toggled?")
}

// MARK: - Test 7: scroll

func testScroll() throws {
    try ensureFront("scroll")
    let source = try receiverSource()
    let center = targetCenter()
    try moveTo(center, source)
    nap(0.15)

    func wheel(_ units: CGScrollEventUnit, _ amount: Int32, phase: Int64 = 0, momentum: Int64 = 0) throws -> CGEvent {
        let event = try made(CGEvent(scrollWheelEvent2Source: source, units: units, wheelCount: 1,
                                     wheel1: amount, wheel2: 0, wheel3: 0), "a scroll event")
        if phase != 0 { event.setIntegerValueField(.scrollWheelEventScrollPhase, value: phase) }
        if momentum != 0 { event.setIntegerValueField(.scrollWheelEventMomentumPhase, value: momentum) }
        return event
    }
    func send(_ arm: String, _ events: [CGEvent]) throws -> [Seen] {
        try guardPoint(cursor())
        let mark = Log.mark()
        for event in events {
            post(event)
            nap(0.016)
        }
        nap(0.2)
        let seen = Log.since(mark).filter { $0.type == .scrollWheel }
        report("scroll", [
            "arm": arm, "sent": events.count, "seen": seen.count,
            "created_continuous": events.first?.getIntegerValueField(.scrollWheelEventIsContinuous) ?? -1,
            "scrolling_dy": seen.map(\.scrollY), "delta_y": seen.map(\.dy), "precise": seen.map(\.precise),
            "phase": seen.map { Int($0.phase) }, "momentum": seen.map { Int($0.momentumPhase) },
            "inverted": seen.first?.inverted ?? false,
        ])
        return seen
    }

    let line = try send("line", [try wheel(.line, -3)])
    verdict("scroll.line", pass(line.count == 1 && line.allSatisfy { !$0.precise }), "one line event, not precise")
    let pixel = try send("pixel", [try wheel(.pixel, -40)])
    verdict("scroll.pixel", pass(pixel.count == 1 && pixel.allSatisfy(\.precise)), "one pixel event, precise")

    // kCGScrollPhase: began 1, changed 2, ended 4. kCGMomentumScrollPhase:
    // begin 1, continue 2, end 3. One gesture: the fingers, then the coast.
    var gesture = [try wheel(.pixel, -2, phase: 1)]
    for _ in 0..<6 { gesture.append(try wheel(.pixel, -6, phase: 2)) }
    gesture.append(try wheel(.pixel, 0, phase: 4))
    gesture.append(try wheel(.pixel, -8, momentum: 1))
    for amount: Int32 in [-7, -6, -5, -4, -3, -2] { gesture.append(try wheel(.pixel, amount, momentum: 2)) }
    gesture.append(try wheel(.pixel, 0, momentum: 3))
    let seen = try send("gesture", gesture)
    let phases = Set(seen.map(\.phase))
    let momentum = Set(seen.map(\.momentumPhase))
    let wanted: Set<UInt> = [NSEvent.Phase.began.rawValue, NSEvent.Phase.changed.rawValue, NSEvent.Phase.ended.rawValue]
    verdict("scroll.phase", pass(wanted.isSubset(of: phases)), "AppKit phases seen \(show(phases.sorted().map { Int($0) }))")
    verdict("scroll.momentum", pass(wanted.isSubset(of: momentum)),
            "AppKit momentum phases seen \(show(momentum.sorted().map { Int($0) }))")
}

// MARK: - Test 8: keys

func testKeys() throws {
    try ensureFront("keys")
    let source = try receiverSource()
    let initialRaw = globalPreference("InitialKeyRepeat")
    let repeatRaw = globalPreference("KeyRepeat")
    let (appDelay, appInterval) = onMain { (NSEvent.keyRepeatDelay, NSEvent.keyRepeatInterval) }
    let delay = initialRaw.map { Double($0) * 0.015 } ?? appDelay
    let interval = repeatRaw.map { Double($0) * 0.015 } ?? appInterval
    report("keys", [
        "initial_key_repeat": initialRaw.map { "\($0)" } ?? "unset",
        "key_repeat": repeatRaw.map { "\($0)" } ?? "unset",
        "delay_s": delay, "interval_s": interval, "appkit_delay_s": appDelay, "appkit_interval_s": appInterval,
    ])

    // Repeats made here, the way the receiver would make them.
    var mark = Log.mark()
    try guardKeys()
    post(try keyEvent(Key.k, down: true, source))
    nap(min(delay, 2.0))
    for _ in 0..<5 {
        let again = try keyEvent(Key.k, down: true, source)
        again.setIntegerValueField(.keyboardEventAutorepeat, value: 1)
        try guardKeys()
        post(again)
        nap(min(interval, 0.5))
    }
    post(try keyEvent(Key.k, down: false, source))
    nap(0.2)
    let repeats = Log.since(mark).filter { $0.type == .keyDown && $0.keyCode == Int(Key.k) }
    report("keys", ["step": "posted_repeats", "keydowns": repeats.count, "is_a_repeat": repeats.map(\.isRepeat)])
    verdict("keys.repeat_flag", pass(repeats.map(\.isRepeat) == [false, true, true, true, true, true]),
            "isARepeat \(show(repeats.map(\.isRepeat)))")

    // Does macOS repeat a held posted key by itself?
    let hold = max(1.0, min(delay + 6 * interval, 3.0))
    mark = Log.mark()
    try tapKey(Key.l, source, hold: hold)
    nap(0.2)
    let held = Log.since(mark).filter { $0.type == .keyDown && $0.keyCode == Int(Key.l) }
    report("keys", ["step": "held_without_repeats", "hold_s": hold, "keydowns": held.count,
                    "marked": held.filter(\.marked).count, "is_a_repeat": held.map(\.isRepeat)])
    verdict("keys.no_system_repeat", pass(held.count == 1),
            held.count == 1 ? "macOS does not repeat a held posted key, so the receiver must"
                : "macOS repeated a held posted key \(held.count - 1) times, so the receiver must not")

    var arrived = 0
    for (name, code) in [("print_screen", Key.f13), ("scroll_lock", Key.f14), ("pause", Key.f15)] {
        mark = Log.mark()
        try tapKey(code, source)
        nap(0.25)
        let down = Log.since(mark).first { $0.type == .keyDown }
        if down?.keyCode == Int(code) { arrived += 1 }
        report("keys", ["step": "unmapped", "linux_key": name, "keycode": Int(code),
                        "seen_keycode": down?.keyCode ?? -1, "chars": scalars(down?.chars ?? "")])
    }
    verdict("keys.f13_f15", pass(arrived == 3), "\(arrived) of 3 arrived with their keycode")
    verdict("keys.f13_f15_side_effects", "MANUAL", "did brightness or anything else change while F13 to F15 went out?")
}

// MARK: - Test 9: media (opt-in)

struct Volume: Equatable {
    var level: Int
    var muted: Bool
}

@discardableResult
func run(_ path: String, _ args: [String]) -> (Int32, String) {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: path)
    process.arguments = args
    let pipe = Pipe()
    process.standardOutput = pipe
    process.standardError = pipe
    do { try process.run() } catch { return (-1, "\(error)") }
    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    process.waitUntilExit()
    return (process.terminationStatus, String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines))
}

// "output volume:44, input volume:58, alert volume:100, output muted:false"
func volumeSettings() -> Volume? {
    let (status, text) = run("/usr/bin/osascript", ["-e", "get volume settings"])
    guard status == 0 else { return nil }
    var fields: [String: String] = [:]
    for part in text.split(separator: ",") {
        let pair = part.split(separator: ":", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
        if pair.count == 2 { fields[pair[0]] = pair[1] }
    }
    guard let level = fields["output volume"].flatMap({ Int($0) }) else { return nil }
    return Volume(level: level, muted: fields["output muted"] == "true")
}

func setVolume(_ volume: Volume) {
    run("/usr/bin/osascript", ["-e", "set volume output volume \(volume.level)",
                               "-e", "set volume output muted \(volume.muted)"])
}

func volumeText(_ volume: Volume?) -> String {
    volume.map { "\($0.level)\($0.muted ? "(muted)" : "")" } ?? "unknown"
}

// Built-in display brightness through the private DisplayServices, read only.
enum Brightness {
    typealias Get = @convention(c) (UInt32, UnsafeMutablePointer<Float>) -> Int32
    static let get: Get? = {
        guard let handle = dlopen("/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices", RTLD_LAZY),
              let symbol = dlsym(handle, "DisplayServicesGetBrightness")
        else { return nil }
        return unsafeBitCast(symbol, to: Get.self)
    }()

    static func builtin() -> Double? {
        guard let get, let id = displayIDs().first(where: { CGDisplayIsBuiltin($0) != 0 }) else { return nil }
        var value: Float = -1
        return get(id, &value) == 0 ? Double(value) : nil
    }
}

func mediaKey(_ key: Int) throws {
    for down in [true, false] {
        let event: CGEvent? = onMain {
            NSEvent.otherEvent(
                with: .systemDefined, location: .zero,
                modifierFlags: NSEvent.ModifierFlags(rawValue: down ? 0xA00 : 0xB00),
                timestamp: 0, windowNumber: 0, context: nil, subtype: 8,
                data1: (key << 16) | ((down ? 0xA : 0xB) << 8), data2: -1)?.cgEvent
        }
        post(try made(event, "a media key event"))
        nap(0.05)
    }
}

func testMedia() throws {
    say("media: volume steps and returns, then brightness steps up and back down. Watch the screen.")
    nap(2)
    let v0 = volumeSettings()
    if let v0 {
        let first = v0.level >= 94 ? MediaKey.soundDown : MediaKey.soundUp
        let second = first == MediaKey.soundUp ? MediaKey.soundDown : MediaKey.soundUp
        try mediaKey(first)
        nap(0.8)
        let v1 = volumeSettings()
        try mediaKey(second)
        nap(0.8)
        let v2 = volumeSettings()
        var restored = "not needed"
        if v2 != v0 {
            setVolume(v0)
            nap(0.3)
            restored = volumeText(volumeSettings())
        }
        report("media", ["step": "volume", "before": volumeText(v0), "after_first": volumeText(v1),
                         "after_second": volumeText(v2), "first_key": first == MediaKey.soundUp ? "up" : "down",
                         "restored_to": restored])
        verdict("media.volume", pass(v1 != nil && v1 != v0), "volume \(volumeText(v0)) -> \(volumeText(v1)) -> \(volumeText(v2))")
    } else {
        verdict("media.volume", "MANUAL", "osascript could not read the output volume; did the volume HUD step?")
        try mediaKey(MediaKey.soundUp)
        nap(0.8)
        try mediaKey(MediaKey.soundDown)
    }

    let b0 = Brightness.builtin()
    try mediaKey(MediaKey.brightnessUp)
    nap(1.0)
    let b1 = Brightness.builtin()
    try mediaKey(MediaKey.brightnessDown)
    nap(1.0)
    let b2 = Brightness.builtin()
    var fix = "not needed"
    if let b0, let b2, abs(b2 - b0) > 0.02 {
        try mediaKey(b2 < b0 ? MediaKey.brightnessUp : MediaKey.brightnessDown)
        nap(1.0)
        fix = Brightness.builtin().map { show($0) } ?? "unknown"
    }
    report("media", ["step": "brightness", "reader": Brightness.get == nil ? "none" : "displayservices",
                     "before": b0 ?? -1, "after_up": b1 ?? -1, "after_down": b2 ?? -1, "fixed_to": fix])
    verdict("media.brightness", "MANUAL", "did the built-in display step up then down? did an external display change?")
}

// MARK: - Test 10: wake (opt-in)

func asleep() -> Bool { CGDisplayIsAsleep(CGMainDisplayID()) != 0 }

func testWake() throws {
    say("wake: in 5 s the display sleeps and the screen may lock. Touch nothing for 15 s, then unlock if needed.")
    nap(5)
    let before = asleep()
    let (status, output) = run("/usr/bin/pmset", ["displaysleepnow"])
    nap(5)
    let slept = asleep()
    var assertion: IOPMAssertionID = 0
    let declared = IOPMAssertionDeclareUserActivity("zflow spike" as CFString, kIOPMUserActiveLocal, &assertion)
    nap(3)
    let afterDeclare = asleep()
    var afterMove = "-"
    if afterDeclare {
        let source = try receiverSource()
        let c = cursor()
        post(try moveEvent(to: CGPoint(x: c.x + 1, y: c.y), delta: CGPoint(x: 1, y: 0), source))
        nap(0.05)
        post(try moveEvent(to: c, delta: CGPoint(x: -1, y: 0), source))
        nap(2)
        afterMove = asleep() ? "1" : "0"
    }
    if assertion != 0 { IOPMAssertionRelease(assertion) }
    report("wake", ["pmset_status": Int(status), "pmset_output": output, "asleep_before": before,
                    "asleep_after_pmset": slept, "declare_return": hex(UInt32(bitPattern: declared)),
                    "asleep_after_declare": afterDeclare, "asleep_after_move": afterMove])
    if !slept {
        verdict("wake.declare_user_activity", "MANUAL", "the main display never reported asleep, so nothing was tested")
    } else {
        verdict("wake.declare_user_activity", pass(!afterDeclare), "asleep after the declaration: \(afterDeclare ? 1 : 0)")
        if afterDeclare { verdict("wake.mouse_move", pass(afterMove == "0"), "asleep after a 1 px move: \(afterMove)") }
    }
    verdict("wake.lock", "MANUAL", "did the screen lock, and did the probe window take input again after unlocking?")
}

// MARK: - feel mode

// Posts "dx dy" lines from stdin 1:1 as cursor moves, clamped to the displays.
func testFeel(seconds: Double) throws {
    let source = try receiverSource()
    Log.setQuiet(true)
    defer { Log.setQuiet(false) }
    say("feel: posting 'dx dy' lines from stdin 1:1 for up to \(Int(seconds)) s")
    var lines = 0, posted = 0, snapped = 0, unparsed = 0
    var totalX = 0.0, totalY = 0.0
    var pending: [UInt8] = []
    var buffer = [UInt8](repeating: 0, count: 4096)
    var poller = pollfd(fd: 0, events: Int16(POLLIN), revents: 0)
    var endedBy = "time"
    let start = now()
    reading: while now() - start < seconds {
        guard poll(&poller, 1, 100) > 0 else { continue }
        let count = read(0, &buffer, buffer.count)
        if count <= 0 { endedBy = "eof"; break reading }
        pending.append(contentsOf: buffer[0..<count])
        while let newline = pending.firstIndex(of: 10) {
            let text = String(decoding: pending[0..<newline], as: UTF8.self)
            pending.removeSubrange(0...newline)
            lines += 1
            let parts = text.split(whereSeparator: { $0 == " " || $0 == "\t" })
            guard parts.count >= 2, let dx = Double(parts[0]), let dy = Double(parts[1]) else {
                unparsed += 1
                continue
            }
            let c = cursor()
            let (target, moved) = snap(CGPoint(x: c.x + dx, y: c.y + dy))
            if moved { snapped += 1 }
            post(try moveEvent(to: target, delta: CGPoint(x: dx, y: dy), source))
            posted += 1
            totalX += abs(dx)
            totalY += abs(dy)
        }
    }
    report("feel", ["lines": lines, "posted": posted, "snapped": snapped, "unparsed": unparsed,
                    "seconds": now() - start, "ended_by": endedBy, "abs_dx_total": totalX, "abs_dy_total": totalY])
    verdict("feel", "MANUAL", "how did 1:1 unaccelerated motion feel: slow, fast, jumpy, fine?")
}

// MARK: - Runner

let plans: [String: String] = [
    "suppress": "4 arms of 8 s (baseline, default source, HID source with suppression 0 and local events permitted, "
        + "Deskflow's process-wide calls); posts 1 px moves every 8 ms while you circle on the trackpad",
    "delta": "moves the cursor inside the window with location and delta fields set in different ways; hands off",
    "click": "single, double and drag clicks on the window, then Finder takes focus and the probe clicks itself back",
    "flags": "posts left Cmd down, clicks and Cmd+A on the window with and without flags, then Cmd up",
    "iso": "types keycodes 10 and 50 under ANSI, ISO and JIS keyboard types; then asks you to press two real keys",
    "caps": "toggles Caps Lock three ways (flagsChanged 57, keyDown 57, IOHIDSetModifierLockState), types a, restores",
    "scroll": "line, pixel and phased plus momentum scroll events over the window",
    "keys": "posted repeats with isARepeat, a held key with no repeats, then F13, F14, F15",
    "media": "opt-in: volume up then down (restored if it drifts), brightness up then down",
    "wake": "opt-in: pmset displaysleepnow, then IOPMAssertionDeclareUserActivity, then a 1 px move",
    "feel": "reads 'dx dy' lines from stdin and posts them as cursor moves 1:1 for --secs (max 55)",
]

final class Runner: @unchecked Sendable {
    let tests: [String]
    let feelSeconds: Double

    init(tests: [String], feelSeconds: Double) {
        self.tests = tests
        self.feelSeconds = feelSeconds
    }

    func runOne(_ name: String) throws {
        switch name {
        case "suppress": try testSuppress()
        case "delta": try testDelta()
        case "click": try testClick()
        case "flags": try testFlags()
        case "iso": try testISO()
        case "caps": try testCaps()
        case "scroll": try testScroll()
        case "keys": try testKeys()
        case "media": try testMedia()
        case "wake": try testWake()
        case "feel": try testFeel(seconds: feelSeconds)
        default: throw Abort("unknown test \(name)")
        }
    }

    func run() {
        do {
            try ensureFront("start")
        } catch {
            verdict("start", "FAIL", "\(error)")
            finish()
            return
        }
        for name in tests {
            Out.line("BEGIN \(name)")
            Watchdog.arm(name)
            do {
                try runOne(name)
            } catch {
                verdict(name, "FAIL", "aborted: \(error)")
            }
            Safety.releaseAll()
            Watchdog.disarm()
            Out.line("END \(name)")
            say("done: \(name)")
            nap(0.5)
        }
        Verdicts.lock.lock()
        let lines = Verdicts.lines
        Verdicts.lock.unlock()
        let count = { (word: String) in lines.filter { $0.split(separator: " ").dropFirst(2).first == Substring(word) }.count }
        Out.line("SUMMARY pass=\(count("PASS")) fail=\(count("FAIL")) manual=\(count("MANUAL"))")
        for line in lines { Out.line("SUMMARY " + line) }
        finish()
    }

    func finish() {
        DispatchQueue.main.async { NSApp.terminate(nil) }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    let runner: Runner
    var monitor: Any?

    init(runner: Runner) {
        self.runner = runner
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let menu = NSMenu()
        let appItem = NSMenuItem()
        menu.addItem(appItem)
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "Quit zflow inject probe", action: #selector(NSApplication.terminate(_:)),
                        keyEquivalent: "q")
        appItem.submenu = appMenu
        NSApp.mainMenu = menu

        let view = TargetView(frame: NSRect(x: 0, y: 0, width: 960, height: 640))
        let window = NSWindow(contentRect: view.frame, styleMask: [.titled, .closable, .miniaturizable],
                              backing: .buffered, defer: false)
        window.title = "zflow inject probe"
        window.isReleasedWhenClosed = false
        window.contentView = view
        window.acceptsMouseMovedEvents = true
        // Stays above Finder so posted clicks and the instructions stay visible.
        window.level = .floating
        window.center()
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(view)
        UI.window = window
        UI.view = view

        monitor = NSEvent.addLocalMonitorForEvents(matching: .any) { event in
            Log.record(event)
            return event
        }
        NSApp.activate()
        let runner = self.runner
        Thread { runner.run() }.start()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationWillTerminate(_ notification: Notification) {
        Safety.releaseAll()
        Out.line("EXIT")
    }
}

let usage = """
usage: zflow-inject-probe [--dry] [--secs N] <test>...

tests, run in the order given:
  suppress delta click flags iso caps scroll keys   (all = these eight)
  media wake                                        (opt-in, never in all)
  feel                                              (reads "dx dy" lines from stdin, --secs N, max 55, default 30)

--dry   print the environment and the plan; posts nothing and opens no window
--help  this text

Launch with open so macOS checks the probe's own Accessibility grant:
  open -n -W --stdout out.txt --stderr err.txt build/zflow-inject-probe.app --args all
"""

@main
struct Probe {
    static func main() {
        setvbuf(stdout, nil, _IOLBF, 0)
        let args = CommandLine.arguments.dropFirst().filter { !$0.hasPrefix("-psn_") }
        var dry = false
        var seconds = 30.0
        var names: [String] = []
        var index = args.startIndex
        while index < args.endIndex {
            let arg = args[index]
            switch arg {
            case "--help", "-h":
                print(usage)
                exit(0)
            case "--dry":
                dry = true
            case "--secs":
                index += 1
                guard index < args.endIndex, let value = Double(args[index]), value > 0 else {
                    fputs("--secs needs a positive number\n", stderr)
                    exit(64)
                }
                seconds = min(value, 55)
            default:
                if arg.hasPrefix("-") {
                    fputs("unknown option \(arg)\n\(usage)\n", stderr)
                    exit(64)
                }
                names += arg == "all" ? ALL_TESTS : [arg]
            }
            index += 1
        }
        if let unknown = names.first(where: { !KNOWN_TESTS.contains($0) }) {
            fputs("unknown test \(unknown)\n\(usage)\n", stderr)
            exit(64)
        }
        if names.isEmpty && !dry {
            print(usage)
            exit(64)
        }

        if dry {
            environmentReport("dry run; the TCC answers belong to whatever launched this process")
            for name in names.isEmpty ? ALL_TESTS + OPT_IN_TESTS + ["feel"] : names {
                Out.line("PLAN \(name) \(plans[name] ?? "?")")
            }
            exit(0)
        }

        environmentReport("app")
        guard CGPreflightPostEventAccess() else {
            let message = "FATAL this app may not post events. Open System Settings > Privacy & Security > "
                + "Accessibility, click +, add build/zflow-inject-probe.app, switch it on, and launch again. "
                + "Each rebuild changes the ad-hoc signature, so remove and re-add it after every build."
            Out.line(message)
            fputs(message + "\n", stderr)
            exit(2)
        }
        Signals.install()
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        let delegate = AppDelegate(runner: Runner(tests: names, feelSeconds: seconds))
        app.delegate = delegate
        withExtendedLifetime(delegate) { app.run() }
    }
}
