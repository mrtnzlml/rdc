import SwiftUI

struct AddConnectionSheet: View {
    @Environment(ConnectionStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    @State private var name = ""
    @State private var apiBase = ""
    @State private var orgId = ""
    @State private var authKind: AuthKind = .token
    @State private var token = ""
    @State private var username = ""
    @State private var password = ""
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("New Connection").font(.headline).padding()
            Form {
                TextField("Name", text: $name)
                TextField("API base", text: $apiBase, prompt: Text("https://example.test/api/v1"))
                TextField("Org ID", text: $orgId)
                Picker("Auth", selection: $authKind) {
                    Text("Token").tag(AuthKind.token)
                    Text("Username / password").tag(AuthKind.password)
                }.pickerStyle(.segmented)
                if authKind == .token {
                    SecureField("API token", text: $token)
                } else {
                    TextField("Username", text: $username)
                    SecureField("Password", text: $password)
                }
            }.formStyle(.grouped)
            if let error { Text(error).foregroundStyle(.red).padding(.horizontal) }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Add") { add() }.keyboardShortcut(.defaultAction).disabled(!isValid)
            }.padding()
        }
        .frame(width: 420)
    }

    private var isValid: Bool {
        guard !name.isEmpty, !apiBase.isEmpty, UInt64(orgId) != nil else { return false }
        return authKind == .token ? !token.isEmpty : (!username.isEmpty && !password.isEmpty)
    }

    private func add() {
        guard let org = UInt64(orgId) else { error = "Org ID must be a number."; return }
        let input = AddConnectionInput(
            name: name, apiBase: apiBase, orgId: org, authKind: authKind,
            token: authKind == .token ? token : nil,
            username: authKind == .password ? username : nil,
            password: authKind == .password ? password : nil)
        if store.addConnection(input) { dismiss() } else { error = store.lastError }
    }
}
