import SwiftUI

@main @MainActor struct SSMCPApp: App {
    @StateObject private var model = ClientModel()
    @Environment(\.scenePhase) private var phase
    var body: some Scene {
        WindowGroup {
            ContentView(model: model)
                .privacySensitive()
                .overlay { if phase != .active { Color(.systemBackground).overlay(Text("SSMCP").font(.title)) } }
                .onAppear { model.setActive(phase == .active) }
                .onChange(of: phase) { model.setActive($0 == .active) }
        }
    }
}
