// EffectCraft Dock launcher (fork). scripts/fork/install-dock-app.sh compiles it into
// ~/Applications/EffectCraft.app. Click: opens the app, building it first only when the code changed
// (scripts/fork/app.sh). While it builds, a small window shows progress; Stop, closing the window,
// Cmd-Q or Quit from the Dock stop the build. A running EffectCraft is never quit or replaced.
//
// The launcher starts EffectCraft itself and stays running (hidden) until it quits: macOS privacy
// permissions (Full Disk Access, Files and Folders) belong to the app that started a process, so
// granting them to EffectCraft.app once covers every rebuilt binary. A launcher that quit right
// away left the binary on its own, without them.

import AppKit

let root = Bundle.main.object(forInfoDictionaryKey: "EffectCraftRoot") as? String ?? ""
let script = root + "/scripts/fork/app.sh"
let log = root + "/target/local/build.log"
let appBinary = root + "/target/local/local/effectcraft"

/// Runs app.sh with `args` and returns its trimmed output (empty on failure).
func run(_ args: [String]) -> String {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: "/bin/sh")
    p.arguments = [script] + args
    let out = Pipe()
    p.standardOutput = out
    p.standardError = FileHandle.nullDevice
    guard (try? p.run()) != nil else { return "" }
    p.waitUntilExit()
    let data = out.fileHandleForReading.readDataToEndOfFile()
    return String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
}

/// Brings the running EffectCraft (a plain executable, found by its path) to the front.
func bringForward() {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
    p.arguments = ["-f", "^\(appBinary)( |$)"]
    let out = Pipe()
    p.standardOutput = out
    guard (try? p.run()) != nil else { return }
    p.waitUntilExit()
    let pids = String(decoding: out.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
    for line in pids.split(separator: "\n") {
        if let pid = pid_t(line), let app = NSRunningApplication(processIdentifier: pid) {
            app.activate()
            return
        }
    }
}

/// The last "Compiling …" / "Finished …" / "error…" line of the build log.
func lastLogLine() -> String {
    guard let text = try? String(contentsOfFile: log, encoding: .utf8) else { return "" }
    for line in text.split(separator: "\n").reversed() {
        let t = line.trimmingCharacters(in: .whitespaces)
        if t.hasPrefix("Compiling") || t.hasPrefix("Finished") || t.hasPrefix("error") {
            return String(t.prefix(80))
        }
    }
    return ""
}

final class Launcher: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var window: NSWindow?
    var detail = NSTextField(labelWithString: "Only what changed since the last build.")
    var build: Process?
    var appProcess: Process?
    var timer: Timer?
    var stopping = false

    func applicationDidFinishLaunching(_ note: Notification) {
        switch run(["--status"]) {
        case "running":
            bringForward()
            NSApp.terminate(nil)
        case "ready":
            launchApp()
        default:
            startBuild()
        }
    }

    func startBuild() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 420, height: 130), styleMask: [.titled, .closable], backing: .buffered, defer: false)
        w.title = "EffectCraft"
        w.delegate = self
        let title = NSTextField(labelWithString: "Building EffectCraft…")
        title.font = .boldSystemFont(ofSize: 14)
        detail.textColor = .secondaryLabelColor
        detail.lineBreakMode = .byTruncatingTail
        let spinner = NSProgressIndicator()
        spinner.style = .spinning
        spinner.controlSize = .small
        spinner.startAnimation(nil)
        let stop = NSButton(title: "Stop", target: self, action: #selector(stopPressed))
        stop.keyEquivalent = "\u{1b}"
        let top = NSStackView(views: [spinner, title])
        let stack = NSStackView(views: [top, detail, stop])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsets(top: 18, left: 20, bottom: 16, right: 20)
        stack.translatesAutoresizingMaskIntoConstraints = false
        w.contentView?.addSubview(stack)
        if let c = w.contentView {
            NSLayoutConstraint.activate([
                stack.leadingAnchor.constraint(equalTo: c.leadingAnchor),
                stack.trailingAnchor.constraint(equalTo: c.trailingAnchor),
                stack.topAnchor.constraint(equalTo: c.topAnchor),
                detail.widthAnchor.constraint(equalToConstant: 380),
            ])
        }
        w.center()
        w.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        window = w

        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        // Build only: launchApp() starts EffectCraft itself once the build ends.
        p.arguments = [script, "--build-only"]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        p.terminationHandler = { [weak self] proc in
            let status = proc.terminationStatus
            DispatchQueue.main.async { self?.buildEnded(status: status) }
        }
        do {
            try p.run()
        } catch {
            fail("Couldn't start the build: \(error.localizedDescription)")
            return
        }
        build = p
        timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            let line = lastLogLine()
            if !line.isEmpty { self?.detail.stringValue = line }
        }
    }

    func buildEnded(status: Int32) {
        timer?.invalidate()
        build = nil
        if stopping {
            NSApp.terminate(nil)
            return
        }
        // The build's own exit status, not `--status`: sources changed while it ran (an agent
        // merging) leave it "stale" although the app built fine, which showed a false failure.
        if status != 0 {
            fail("The EffectCraft build failed.")
            return
        }
        launchApp()
    }

    /// Start EffectCraft as this launcher's child (so it runs with the launcher's privacy
    /// permissions), hide the launcher, and quit when EffectCraft quits.
    func launchApp() {
        window?.orderOut(nil)
        window = nil
        let p = Process()
        p.executableURL = URL(fileURLWithPath: appBinary)
        p.currentDirectoryURL = URL(fileURLWithPath: root)
        let logURL = URL(fileURLWithPath: root + "/target/local/app.log")
        FileManager.default.createFile(atPath: logURL.path, contents: nil)
        if let out = try? FileHandle(forWritingTo: logURL) {
            p.standardOutput = out
            p.standardError = out
        }
        p.terminationHandler = { _ in DispatchQueue.main.async { NSApp.terminate(nil) } }
        do {
            try p.run()
        } catch {
            fail("Couldn't start EffectCraft: \(error.localizedDescription)")
            return
        }
        appProcess = p
        NSApp.setActivationPolicy(.accessory)
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { bringForward() }
    }

    /// A Dock click while EffectCraft runs: bring it to the front.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if appProcess?.isRunning == true { bringForward() }
        return false
    }

    func fail(_ message: String) {
        window?.orderOut(nil)
        let alert = NSAlert()
        alert.messageText = message
        alert.informativeText = "The build log has the details."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Show Log")
        if alert.runModal() == .alertSecondButtonReturn {
            NSWorkspace.shared.open(URL(fileURLWithPath: log))
        }
        NSApp.terminate(nil)
    }

    /// Stop the build (app.sh stops cargo and every rustc it started), then quit.
    func stopBuild() {
        guard let p = build, p.isRunning else { return }
        stopping = true
        detail.stringValue = "Stopping…"
        p.terminate()
    }

    @objc func stopPressed() {
        if build?.isRunning == true { stopBuild() } else { NSApp.terminate(nil) }
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        if build?.isRunning == true {
            stopBuild()
            return false
        }
        return true
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if build?.isRunning == true {
            // Cmd-Q or Quit from the Dock: stop the build first; buildEnded() quits.
            stopBuild()
            return .terminateCancel
        }
        return .terminateNow
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { appProcess == nil }
}

let app = NSApplication.shared
let delegate = Launcher()
app.delegate = delegate
app.setActivationPolicy(.regular)
// A menu with Quit, so Cmd-Q stops the build too.
let menu = NSMenu()
let appItem = NSMenuItem()
appItem.submenu = NSMenu()
appItem.submenu?.addItem(withTitle: "Quit EffectCraft Launcher", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
menu.addItem(appItem)
app.mainMenu = menu
app.run()
