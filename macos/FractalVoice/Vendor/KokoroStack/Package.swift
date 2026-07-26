// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "KokoroStack",
    platforms: [.macOS(.v15)],
    products: [
        // Static linking ensures MLX is present exactly once in the app.
        .library(name: "KokoroSwift", targets: ["KokoroSwift"])
    ],
    dependencies: [
        .package(
            url: "https://github.com/ml-explore/mlx-swift",
            exact: "0.31.3"
        ),
        .package(
            url: "https://github.com/mlalma/MLXUtilsLibrary.git",
            exact: "0.0.6"
        )
    ],
    targets: [
        .target(
            name: "MisakiSwift",
            dependencies: [
                .product(name: "MLX", package: "mlx-swift"),
                .product(name: "MLXNN", package: "mlx-swift"),
                .product(name: "MLXUtilsLibrary", package: "MLXUtilsLibrary")
            ],
            resources: [.copy("Resources")]
        ),
        .target(
            name: "KokoroSwift",
            dependencies: [
                "MisakiSwift",
                .product(name: "MLX", package: "mlx-swift"),
                .product(name: "MLXNN", package: "mlx-swift"),
                .product(name: "MLXRandom", package: "mlx-swift"),
                .product(name: "MLXFFT", package: "mlx-swift"),
                .product(name: "MLXUtilsLibrary", package: "MLXUtilsLibrary")
            ],
            resources: [.copy("Resources")]
        )
    ]
)
