// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "FractalVoice",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "FractalVoice", targets: ["FractalVoice"])
    ],
    targets: [
        .executableTarget(
            name: "FractalVoice",
            path: "Sources/FractalVoice"
        ),
        .testTarget(
            name: "FractalVoiceTests",
            dependencies: ["FractalVoice"],
            path: "Tests/FractalVoiceTests"
        )
    ],
    swiftLanguageModes: [.v5]
)
