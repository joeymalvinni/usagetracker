import Foundation

enum JSONHelpers {
    static func object(from data: Data) throws -> [String: Any] {
        let value = try JSONSerialization.jsonObject(with: data)
        return value as? [String: Any] ?? [:]
    }

    static func object(from line: String) -> [String: Any]? {
        guard let data = line.data(using: .utf8) else {
            return nil
        }
        return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    }

    static func string(_ dictionary: [String: Any], _ key: String) -> String? {
        dictionary[key] as? String
    }

    static func int(_ dictionary: [String: Any], _ key: String) -> Int {
        if let int = dictionary[key] as? Int {
            return int
        }
        if let double = dictionary[key] as? Double {
            return Int(double)
        }
        if let string = dictionary[key] as? String, let int = Int(string) {
            return int
        }
        return 0
    }

    static func bool(_ dictionary: [String: Any], _ key: String) -> Bool? {
        if let bool = dictionary[key] as? Bool {
            return bool
        }
        if let int = dictionary[key] as? Int {
            return int != 0
        }
        if let string = dictionary[key] as? String {
            switch string.lowercased() {
            case "true", "yes", "1":
                return true
            case "false", "no", "0":
                return false
            default:
                return nil
            }
        }
        return nil
    }

    static func double(_ dictionary: [String: Any], _ key: String) -> Double? {
        if let double = dictionary[key] as? Double {
            return double
        }
        if let int = dictionary[key] as? Int {
            return Double(int)
        }
        if let string = dictionary[key] as? String, let double = Double(string) {
            return double
        }
        return nil
    }

    static func parseISODate(_ string: String) -> Date? {
        let withFractional = ISO8601DateFormatter()
        withFractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let withoutFractional = ISO8601DateFormatter()
        withoutFractional.formatOptions = [.withInternetDateTime]
        return withFractional.date(from: string) ?? withoutFractional.date(from: string)
    }

    static func parseDay(_ string: String, calendar: Calendar = .current) -> Date? {
        var components = DateComponents()
        let pieces = string.split(separator: "-").compactMap { Int($0) }
        guard pieces.count == 3 else {
            return nil
        }
        components.calendar = calendar
        components.year = pieces[0]
        components.month = pieces[1]
        components.day = pieces[2]
        return calendar.date(from: components)
    }
}
