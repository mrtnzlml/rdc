import Foundation
import UserNotifications

extension Notification.Name {
    static let newConnection = Notification.Name("ai.rossum.local.newConnection")
    static let openExisting = Notification.Name("ai.rossum.local.openExisting")
}

enum SyncNotifications {
    static func requestAuthorization() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }
    @MainActor static func post(title: String, body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        let req = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(req)
    }
}
