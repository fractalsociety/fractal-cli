// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "FractalVoice",
    platforms: [.macOS("15.0")],
    products: [
        .executable(name: "FractalVoice", targets: ["FractalVoice"])
    ],
    dependencies: [
        .package(path: "Vendor/KokoroStack"),
        .package(
            url: "https://github.com/ml-explore/mlx-swift",
            exact: "0.31.3"
        )
    ],
    targets: [
        .executableTarget(
            name: "FractalVoice",
            dependencies: [
                .product(name: "KokoroSwift", package: "KokoroStack"),
                .product(name: "MLX", package: "mlx-swift")
            ],
            path: "Sources/FractalVoice",
            resources: [
                .copy("Resources/ChatGPTVoiceIcon.png")
            ]
        ),
        .testTarget(
            name: "FractalVoiceTests",
            dependencies: ["FractalVoice"],
            path: "Tests/FractalVoiceTests"
        )
    ],
    swiftLanguageModes: [.v5]
)
