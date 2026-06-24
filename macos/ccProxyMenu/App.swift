import AppKit
import ServiceManagement
import SwiftUI

struct ProxyConfig: Codable, Equatable {
    var port: String = "48317"
    var host: String = "127.0.0.1"
    var upstreamProxyURL: String = "http://127.0.0.1:6789"
    var authMode: String = "codex"
    var defaultInstructions: String = "You are a helpful assistant."
    var adminPassword: String = ""
    var autoStart: Bool = true
}

final class ProxyController: ObservableObject {
    @Published var config = ProxyConfig()
    @Published var isRunning = false
    @Published var statusText = "已停止"
    @Published var lastError = ""

    private var process: Process?
    private var statusTimer: Timer?
    private let localSession: URLSession

    let appSupportDir: URL
    let configURL: URL
    let logURL: URL

    init() {
        let fm = FileManager.default
        let appSupport = fm.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/ccProxy", isDirectory: true)
        let logs = fm.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Logs/ccProxy", isDirectory: true)

        try? fm.createDirectory(at: appSupport, withIntermediateDirectories: true)
        try? fm.createDirectory(at: logs, withIntermediateDirectories: true)

        appSupportDir = appSupport
        configURL = appSupport.appendingPathComponent("config.json")
        logURL = logs.appendingPathComponent("ccproxy.log")

        let sessionConfig = URLSessionConfiguration.ephemeral
        sessionConfig.connectionProxyDictionary = [:]
        sessionConfig.timeoutIntervalForRequest = 2
        sessionConfig.timeoutIntervalForResource = 3
        sessionConfig.waitsForConnectivity = false
        localSession = URLSession(configuration: sessionConfig)

        loadConfig()
        startStatusTimer()
    }

    var endpoint: String {
        "http://\(config.host):\(config.port)/v1/messages"
    }

    var healthURL: URL? {
        URL(string: "http://\(config.host):\(config.port)/health")
    }

    var authLoginURL: URL? {
        URL(string: "http://\(config.host):\(config.port)/auth/login")
    }

    var dashboardURL: URL? {
        URL(string: "http://\(config.host):\(config.port)/admin")
    }

    var menuTitle: String {
        isRunning ? "ccP●" : "ccP○"
    }

    var statusIconName: String { "logo" }

    func loadConfig() {
        guard let data = try? Data(contentsOf: configURL),
              let loaded = try? JSONDecoder().decode(ProxyConfig.self, from: data) else {
            saveConfig()
            return
        }
        config = loaded
    }

    func saveConfig() {
        guard let data = try? JSONEncoder.pretty.encode(config) else { return }
        try? data.write(to: configURL, options: [.atomic])
    }

    func start() {
        if process?.isRunning == true {
            refreshStatus()
            return
        }

        saveConfig()
        lastError = ""

        guard let resources = Bundle.main.resourceURL else {
            statusText = "缺少应用资源"
            lastError = statusText
            return
        }

        let serverURL = resources.appendingPathComponent("ccproxy-server")

        let p = Process()
        p.executableURL = serverURL
        p.arguments = []
        p.currentDirectoryURL = appSupportDir
        p.environment = processEnvironment(resources: resources)

        let out = Pipe()
        let err = Pipe()
        p.standardOutput = out
        p.standardError = err
        pipeToLog(out)
        pipeToLog(err)

        p.terminationHandler = { [weak self] process in
            Task { @MainActor in
                guard let self else { return }
                if self.process === process {
                    self.process = nil
                    self.isRunning = false
                    self.statusText = process.terminationStatus == 0 ? "已停止" : "已退出 \(process.terminationStatus)"
                }
            }
        }

        do {
            try appendLog("\n--- ccProxy start \(Date()) ---\n")
            try p.run()
            process = p
            isRunning = true
            statusText = "正在启动"
            refreshStatus()
        } catch {
            statusText = "启动失败"
            lastError = error.localizedDescription
            try? appendLog("Failed to start: \(error.localizedDescription)\n")
        }
    }

    func stop() {
        guard let process else {
            isRunning = false
            statusText = "已停止"
            return
        }
        process.terminate()
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            if process.isRunning {
                process.interrupt()
            }
        }
        self.process = nil
        isRunning = false
        statusText = "已停止"
    }

    func restart() {
        stop()
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
            self.start()
        }
    }

    func checkHealth(completion: @escaping (String) -> Void) {
        guard let healthURL else {
            let message = "健康检查地址无效"
            statusText = message
            lastError = message
            completion(message)
            return
        }

        statusText = "检查中"
        Task {
            let message: String
            do {
                var request = URLRequest(url: healthURL)
                request.timeoutInterval = 2
                let (data, response) = try await localSession.data(for: request)
                let statusCode = (response as? HTTPURLResponse)?.statusCode ?? 0
                let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
                let ok = json?["ok"] as? Bool

                if (200..<300).contains(statusCode), ok != false {
                    message = "健康"
                } else {
                    message = statusCode > 0 ? "异常（HTTP \(statusCode)）" : "异常"
                }
            } catch {
                message = "不可达：\(error.localizedDescription)"
            }

            await MainActor.run {
                isRunning = message == "健康"
                statusText = message
                if message != "健康" {
                    lastError = message
                }
                completion(message)
            }
        }
    }

    func openAuthLogin() {
        guard let authLoginURL else { return }
        Task {
            do {
                let (data, _) = try await localSession.data(from: authLoginURL)
                if let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                   let urlString = json["authorization_url"] as? String,
                   let url = URL(string: urlString) {
                    _ = await MainActor.run { NSWorkspace.shared.open(url) }
                } else {
                    _ = await MainActor.run { NSWorkspace.shared.open(authLoginURL) }
                }
            } catch {
                await MainActor.run {
                    lastError = error.localizedDescription
                    NSWorkspace.shared.open(authLoginURL)
                }
            }
        }
    }

    func openDashboard() {
        guard let dashboardURL else { return }
        NSWorkspace.shared.open(dashboardURL)
    }

    func openLogs() {
        NSWorkspace.shared.open(logURL)
    }

    func copyEndpoint() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(endpoint, forType: .string)
    }

    func refreshStatus() {
        guard let healthURL else {
            isRunning = process?.isRunning == true
            statusText = isRunning ? "Running" : "Stopped"
            return
        }

        Task {
            do {
                var request = URLRequest(url: healthURL)
                request.timeoutInterval = 2
                let (data, _) = try await localSession.data(for: request)
                if let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                   let ok = json["ok"] as? Bool,
                   ok {
                    await MainActor.run {
                        isRunning = true
                        statusText = "运行中"
                    }
                }
            } catch {
                await MainActor.run {
                    isRunning = process?.isRunning == true
                    statusText = isRunning ? "正在启动" : "已停止"
                }
            }
        }
    }

    private func startStatusTimer() {
        statusTimer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refreshStatus() }
        }
    }

    private func processEnvironment(resources: URL) -> [String: String] {
        var env = ProcessInfo.processInfo.environment
        env["HOST"] = config.host
        env["PORT"] = config.port
        env["OPENAI_AUTH_MODE"] = config.authMode
        env["DEFAULT_INSTRUCTIONS"] = config.defaultInstructions
        env["UPSTREAM_PROXY_URL"] = config.upstreamProxyURL
        env["CODEX_AUTH_FILE"] = fmHome(".codex/auth.json")
        env["DB_PATH"] = appSupportDir.appendingPathComponent("ccproxy.db").path
        if !config.adminPassword.isEmpty {
            env["ADMIN_API_KEY"] = config.adminPassword
        }
        env["PATH"] = "\(resources.path):/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        return env
    }

    private func fmHome(_ relative: String) -> String {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(relative).path
    }

    private func pipeToLog(_ pipe: Pipe) {
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            Task { @MainActor in
                try? self?.appendLog(data)
            }
        }
    }

    private func appendLog(_ string: String) throws {
        try appendLog(Data(string.utf8))
    }

    private func appendLog(_ data: Data) throws {
        if !FileManager.default.fileExists(atPath: logURL.path) {
            FileManager.default.createFile(atPath: logURL.path, contents: nil)
        }
        let handle = try FileHandle(forWritingTo: logURL)
        try handle.seekToEnd()
        try handle.write(contentsOf: data)
        try handle.close()
    }
}

extension JSONEncoder {
    static var pretty: JSONEncoder {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        return encoder
    }
}

enum LaunchAtLogin {
    static var isEnabled: Bool {
        if #available(macOS 13.0, *) {
            return SMAppService.mainApp.status == .enabled
        }
        return false
    }

    static func setEnabled(_ enabled: Bool) throws {
        if #available(macOS 13.0, *) {
            if enabled {
                if SMAppService.mainApp.status != .enabled {
                    try SMAppService.mainApp.register()
                }
            } else if SMAppService.mainApp.status == .enabled {
                try SMAppService.mainApp.unregister()
            }
        }
    }
}

struct SettingsView: View {
    @ObservedObject var controller: ProxyController
    @State private var launchAtLogin: Bool
    @State private var launchAtLoginError = ""

    init(controller: ProxyController) {
        self.controller = controller
        _launchAtLogin = State(initialValue: LaunchAtLogin.isEnabled)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                GroupBox("服务配置") {
                    VStack(alignment: .leading, spacing: 10) {
                        TextField("监听地址", text: $controller.config.host)
                        TextField("监听端口", text: $controller.config.port)
                        TextField("上游代理 URL", text: $controller.config.upstreamProxyURL)
                        TextField("认证模式", text: $controller.config.authMode)
                        SecureField("管理后台密码", text: $controller.config.adminPassword)
                        TextField("默认系统提示词", text: $controller.config.defaultInstructions, axis: .vertical)
                            .lineLimit(3...8)
                    }
                }

                GroupBox("启动选项") {
                    VStack(alignment: .leading, spacing: 10) {
                        Toggle("打开 ccProxy 时自动启动本地代理", isOn: $controller.config.autoStart)
                        Toggle("随系统启动 ccProxy", isOn: Binding(
                            get: { launchAtLogin },
                            set: { value in
                                do {
                                    try LaunchAtLogin.setEnabled(value)
                                    launchAtLogin = value
                                    launchAtLoginError = ""
                                } catch {
                                    launchAtLogin = LaunchAtLogin.isEnabled
                                    launchAtLoginError = error.localizedDescription
                                }
                            }
                        ))
                        if !launchAtLoginError.isEmpty {
                            Text("设置开机启动失败：\(launchAtLoginError)")
                                .foregroundStyle(.red)
                                .font(.caption)
                        }
                    }
                }

                GroupBox("使用说明") {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Claude / Anthropic 兼容地址：")
                            .font(.headline)
                        Text(controller.endpoint)
                            .textSelection(.enabled)
                            .font(.system(.body, design: .monospaced))
                        Text("点击菜单栏图标可启动、停止、重启代理，也可以复制 endpoint、打开日志、打开管理后台或发起网页登录授权。")
                        Text("管理后台地址：http://\(controller.config.host):\(controller.config.port)/admin")
                        Text("模型名支持 reasoning 别名，例如 gpt-5.5-high、gpt-5.5 xhigh。")
                    }
                    .font(.callout)
                }

                GroupBox("注意事项") {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("这个 app 已内置 Rust 代理服务，不需要你另外打开终端。")
                        Text("不强制安装 Codex CLI；如果你已经用 Codex CLI 登录过，会复用 ~/.codex/auth.json。")
                        Text("如果没有登录，需要先点“打开网页登录授权”。授权成功后 token 会写入 ~/.codex/auth.json。")
                        Text("仍然需要你的 OpenAI / ChatGPT 账号具备 Codex 可用权限。")
                        Text("如果国内网络直连不可用，保持“上游代理 URL”为 http://127.0.0.1:6789 或你的本机代理地址。")
                    }
                    .font(.callout)
                }

                HStack {
                    Button("保存") {
                        controller.saveConfig()
                    }
                    Button("保存并重启代理") {
                        controller.saveConfig()
                        controller.restart()
                    }
                    Button("复制 Endpoint") {
                        controller.copyEndpoint()
                    }
                }
            }
            .padding(20)
        }
        .frame(width: 620, height: 650)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private let controller = ProxyController()
    private var statusItem: NSStatusItem?
    private var settingsWindow: NSWindow?
    private var titleTimer: Timer?

    private let statusMenuItem = NSMenuItem(title: "ccProxy：已停止", action: nil, keyEquivalent: "")
    private let endpointMenuItem = NSMenuItem(title: "", action: nil, keyEquivalent: "")
    private let startMenuItem = NSMenuItem(title: "启动代理", action: #selector(startProxy), keyEquivalent: "s")
    private let stopMenuItem = NSMenuItem(title: "停止代理", action: #selector(stopProxy), keyEquivalent: "x")
    private lazy var cachedStatusIcon: NSImage = statusIcon(named: controller.statusIconName)

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)

        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        item.button?.title = ""
        item.button?.imagePosition = .imageOnly
        item.button?.image = cachedStatusIcon
        item.button?.toolTip = "ccProxy 本地代理"
        item.menu = buildMenu()
        item.isVisible = true
        statusItem = item

        titleTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.updateStatusItem()
            }
        }

        if controller.config.autoStart {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in
                self?.controller.start()
                self?.updateStatusItem()
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        controller.stop()
    }

    func menuWillOpen(_ menu: NSMenu) {
        controller.refreshStatus()
        updateStatusItem()
    }

    private func buildMenu() -> NSMenu {
        let menu = NSMenu()
        menu.delegate = self

        statusMenuItem.isEnabled = false
        endpointMenuItem.isEnabled = false
        menu.addItem(statusMenuItem)
        menu.addItem(endpointMenuItem)
        menu.addItem(.separator())

        startMenuItem.target = self
        stopMenuItem.target = self
        menu.addItem(startMenuItem)
        menu.addItem(stopMenuItem)
        menu.addItem(menuItem("重启代理", action: #selector(restartProxy), key: "r"))
        menu.addItem(.separator())

        menu.addItem(menuItem("打开网页登录授权", action: #selector(openAuthLogin)))
        menu.addItem(menuItem("打开管理后台", action: #selector(openDashboard)))
        menu.addItem(menuItem("检查健康状态", action: #selector(checkHealth)))
        menu.addItem(menuItem("复制 Endpoint", action: #selector(copyEndpoint)))
        menu.addItem(menuItem("打开日志", action: #selector(openLogs)))
        menu.addItem(.separator())

        menu.addItem(menuItem("设置与说明", action: #selector(openSettings), key: ","))
        menu.addItem(menuItem("退出", action: #selector(quit), key: "q"))

        return menu
    }

    private func menuItem(_ title: String, action: Selector, key: String = "") -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        item.target = self
        return item
    }

    private func updateStatusItem() {
        statusItem?.isVisible = true
        statusItem?.button?.title = ""
        if statusItem?.button?.image == nil {
            statusItem?.button?.image = cachedStatusIcon
        }
        statusMenuItem.title = "ccProxy：\(controller.statusText)"
        endpointMenuItem.title = controller.endpoint
        startMenuItem.isEnabled = !controller.isRunning
        stopMenuItem.isEnabled = controller.isRunning
    }

    private func statusIcon(named name: String) -> NSImage {
        if let logoURL = Bundle.main.url(forResource: "logo", withExtension: "svg"),
           let image = NSImage(contentsOf: logoURL) {
            image.isTemplate = true
            image.size = NSSize(width: 18, height: 18)
            return image
        }

        if let image = NSImage(systemSymbolName: name, accessibilityDescription: "ccProxy") {
            image.isTemplate = true
            image.size = NSSize(width: 18, height: 18)
            return image
        }

        let image = NSImage(size: NSSize(width: 18, height: 18))
        image.lockFocus()
        NSColor.labelColor.setStroke()
        let path = NSBezierPath(ovalIn: NSRect(x: 2, y: 2, width: 14, height: 14))
        path.lineWidth = 2
        path.stroke()
        image.unlockFocus()
        image.isTemplate = true
        return image
    }

    @objc private func startProxy() {
        controller.start()
        updateStatusItem()
    }

    @objc private func stopProxy() {
        controller.stop()
        updateStatusItem()
    }

    @objc private func restartProxy() {
        controller.restart()
        updateStatusItem()
    }

    @objc private func openAuthLogin() {
        controller.openAuthLogin()
    }

    @objc private func openDashboard() {
        controller.openDashboard()
    }

    @objc private func checkHealth() {
        controller.checkHealth { [weak self] message in
            self?.updateStatusItem()
            let alert = NSAlert()
            alert.messageText = "健康检查：\(message)"
            alert.informativeText = self?.controller.healthURL?.absoluteString ?? ""
            alert.alertStyle = message == "健康" ? .informational : .warning
            alert.runModal()
        }
    }

    @objc private func copyEndpoint() {
        controller.copyEndpoint()
    }

    @objc private func openLogs() {
        controller.openLogs()
    }

    @objc private func openSettings() {
        if settingsWindow == nil {
            let hosting = NSHostingController(rootView: SettingsView(controller: controller))
            let window = NSWindow(contentViewController: hosting)
            window.title = "ccProxy 设置与说明"
            window.styleMask = [.titled, .closable, .miniaturizable]
            window.isReleasedWhenClosed = false
            settingsWindow = window
        }

        settingsWindow?.center()
        settingsWindow?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    @objc private func quit() {
        controller.stop()
        NSApp.terminate(nil)
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.run()
