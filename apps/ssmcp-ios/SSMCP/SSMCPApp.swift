import SwiftUI

@main @MainActor struct SSMCPApp: App {
    @StateObject private var relay = IOSRelaySession()
    @StateObject private var model = ClientModel()
    @Environment(\.scenePhase) private var phase
    var body: some Scene {
        WindowGroup {
            DirectSessionView(relay: relay, advanced: model)
                .privacySensitive()
                .overlay { if phase != .active { Color(.systemBackground).overlay(Text("SSMCP").font(.title)) } }
                .onAppear { model.setActive(phase == .active) }
                .onChange(of: phase) { next in
                    model.setActive(next == .active)
                    if next == .background && (relay.connecting || !relay.clientID.isEmpty) {
                        relay.end("应用进入后台，此设备会话已结束。")
                    }
                }
        }
    }
}
