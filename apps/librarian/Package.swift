// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "librarian",
    platforms: [.macOS("26.0")],
    targets: [
        .executableTarget(name: "librarian", path: "Sources/librarian")
    ]
)
