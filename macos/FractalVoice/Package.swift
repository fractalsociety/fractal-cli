// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "FractalVoice",
    platforms: [.macOS("13.4")],
    products: [
        .executable(name: "FractalVoice", targets: ["FractalVoice"])
    ],
    dependencies: [
        .package(
            url: "https://github.com/moonshine-ai/moonshine-swift.git",
            exact: "0.0.73"
        )
    ],
    targets: [
        .executableTarget(
            name: "FractalVoice",
            dependencies: [
                .product(name: "MoonshineVoice", package: "moonshine-swift")
            ],
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
