// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "usagetracker",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .executable(name: "usage", targets: ["UsageCLI"]),
        .library(name: "UsageCore", targets: ["UsageCore"]),
        .library(name: "UsageStore", targets: ["UsageStore"]),
        .library(name: "UsageProviders", targets: ["UsageProviders"])
    ],
    targets: [
        .target(name: "UsageCore"),
        .target(
            name: "UsageStore",
            dependencies: ["UsageCore"],
            linkerSettings: [
                .linkedLibrary("sqlite3")
            ]
        ),
        .target(
            name: "UsageProviders",
            dependencies: ["UsageCore"]
        ),
        .executableTarget(
            name: "UsageCLI",
            dependencies: ["UsageCore", "UsageStore", "UsageProviders"]
        )
    ]
)
