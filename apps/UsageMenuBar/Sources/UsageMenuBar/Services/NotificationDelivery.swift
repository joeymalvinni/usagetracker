import Foundation
import UserNotifications

struct NotificationDeliveryResult: Sendable {
    let deliveredIDs: [Int64]
    let errors: [String]
}

actor NotificationDelivery {
    private let center: UNUserNotificationCenter

    init(center: UNUserNotificationCenter = .current()) {
        self.center = center
    }

    func authorizationStatus() async -> UNAuthorizationStatus {
        await center.notificationSettings().authorizationStatus
    }

    func requestAuthorization() async throws -> Bool {
        try await center.requestAuthorization(options: [.alert, .sound, .badge])
    }

    func deliver(_ notifications: [PendingNotification]) async -> NotificationDeliveryResult {
        var delivered = [Int64]()
        var errors = [String]()
        for notification in notifications {
            let content = UNMutableNotificationContent()
            content.title = notification.title
            content.body = notification.body
            content.sound = .default
            let request = UNNotificationRequest(
                identifier: "usage-alert-\(notification.id)",
                content: content,
                trigger: nil
            )
            do {
                try await center.add(request)
                delivered.append(notification.id)
            } catch {
                errors.append(error.localizedDescription)
            }
        }
        return NotificationDeliveryResult(deliveredIDs: delivered, errors: errors)
    }
}
