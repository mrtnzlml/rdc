import SwiftUI

struct EditCredentialsSheet: View {
    @Environment(ConnectionStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    let connection: ConnectionSummary

    @State private var authKind: AuthKind = .token
    @State private var token = ""
    @State private var username = ""
    @State private var password = ""
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Edit Credentials — \(connection.name)").font(.headline).padding()
            Form {
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
                Button("Save") { save() }.keyboardShortcut(.defaultAction).disabled(!isValid)
            }.padding()
        }
        .frame(width: 420)
        .onAppear { authKind = connection.authKind }
    }

    private var isValid: Bool {
        authKind == .token ? !token.isEmpty : (!username.isEmpty && !password.isEmpty)
    }

    private func save() {
        let input = EditCredentialsInput(
            authKind: authKind,
            token: authKind == .token ? token : nil,
            username: authKind == .password ? username : nil,
            password: authKind == .password ? password : nil)
        if store.editCredentials(folder: URL(fileURLWithPath: connection.folder), input) { dismiss() }
        else { error = store.lastError }
    }
}
