import Darwin
import Foundation

enum Socket {
    static func canConnect(
        path: String,
        timeout: TimeInterval,
        isCancelled: @escaping @Sendable () -> Bool = { false }
    ) -> Bool {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        do {
            try connect(
                fd: fd,
                path: path,
                deadline: Date().addingTimeInterval(timeout),
                isCancelled: isCancelled
            )
            return true
        } catch {
            return false
        }
    }

    static func line(
        path: String,
        request: String,
        timeout: TimeInterval,
        isCancelled: @escaping @Sendable () -> Bool = { false }
    ) throws -> String {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw DaemonError.transport(errno) }
        defer { close(fd) }
        let deadline = Date().addingTimeInterval(timeout)

        try connect(fd: fd, path: path, deadline: deadline, isCancelled: isCancelled)

        var out = Array(request.utf8)
        while !out.isEmpty {
            if isCancelled() { throw CancellationError() }
            let sent = out.withUnsafeBytes { write(fd, $0.baseAddress!, out.count) }
            if sent > 0 {
                out.removeFirst(sent)
            } else if errno == EAGAIN || errno == EWOULDBLOCK {
                try wait(
                    fd: fd,
                    events: Int16(POLLOUT),
                    deadline: deadline,
                    isCancelled: isCancelled
                )
            } else if errno != EINTR {
                throw DaemonError.transport(errno)
            }
        }

        var data = [UInt8](), buf = [UInt8](repeating: 0, count: 4096)
        while true {
            if isCancelled() { throw CancellationError() }
            let n = read(fd, &buf, buf.count)
            if n > 0 {
                if let i = buf[..<n].firstIndex(of: 10) { data += buf[..<i]; break }
                data += buf[..<n]
            } else if n == 0 {
                throw DaemonError.closed
            } else if errno == EAGAIN || errno == EWOULDBLOCK {
                try wait(
                    fd: fd,
                    events: Int16(POLLIN),
                    deadline: deadline,
                    isCancelled: isCancelled
                )
            } else if errno != EINTR {
                throw DaemonError.transport(errno)
            }
        }
        return String(decoding: data, as: UTF8.self)
    }

    private static func connect(
        fd: Int32,
        path: String,
        deadline: Date,
        isCancelled: @escaping @Sendable () -> Bool
    ) throws {
        if isCancelled() { throw CancellationError() }
        let flags = fcntl(fd, F_GETFL, 0)
        guard flags >= 0, fcntl(fd, F_SETFL, flags | O_NONBLOCK) >= 0 else { throw DaemonError.transport(errno) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8)
        let maxPathBytes = MemoryLayout.size(ofValue: addr.sun_path) - 1
        guard pathBytes.count <= maxPathBytes else { throw DaemonError.pathTooLong(path, maxPathBytes) }
        let bytes = pathBytes + [0]
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: bytes) }
        let len = socklen_t(MemoryLayout<sa_family_t>.size + bytes.count)
        let connected = withUnsafePointer(to: &addr) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, len) } }
        if connected != 0 {
            let code = errno
            guard code == EINPROGRESS || code == EWOULDBLOCK else { throw DaemonError.transport(code) }
            try wait(
                fd: fd,
                events: Int16(POLLOUT),
                deadline: deadline,
                isCancelled: isCancelled
            )
            var error = Int32(0)
            var length = socklen_t(MemoryLayout<Int32>.size)
            guard getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) == 0 else { throw DaemonError.transport(errno) }
            guard error == 0 else { throw DaemonError.transport(error) }
        }
    }

    private static func wait(
        fd: Int32,
        events: Int16,
        deadline: Date,
        isCancelled: @escaping @Sendable () -> Bool
    ) throws {
        while true {
            if isCancelled() { throw CancellationError() }
            var pollFd = pollfd(fd: fd, events: events, revents: 0)
            let remaining = remainingMilliseconds(until: deadline)
            guard remaining > 0 else { throw DaemonError.timeout }
            // Bound cancellation latency even when the request deadline is long.
            let result = poll(&pollFd, 1, min(remaining, 100))
            if result > 0 { return }
            if result == 0 { continue }
            if errno != EINTR { throw DaemonError.transport(errno) }
        }
    }

    private static func remainingMilliseconds(until deadline: Date) -> Int32 {
        let remaining = deadline.timeIntervalSinceNow
        guard remaining > 0 else { return 0 }
        return min(Int32.max, max(1, Int32((remaining * 1000).rounded(.up))))
    }
}
