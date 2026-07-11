// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "UsageMenuBar",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "UsageMenuBar",
            resources: [.copy("Resources")]
        ),
        .testTarget(
            name: "UsageMenuBarTests",
            dependencies: ["UsageMenuBar"]
        )
    ]
)
